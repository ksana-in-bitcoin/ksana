# Design decision log (DECISIONS)

`docs/SPEC.md` is the single source of requirements; SPEC v2 of 2026-10-03 merged the rules settled by D-001–D-086 back into its body, and since then this file records only rationale, trade-offs and process,
while the SPEC is authoritative for the rules. This file records two kinds of content (SPEC §0, §12):

1. Contradictions, gaps or unimplementable points in the spec: the problem, the proposed solution, and the (most conservative) implementation currently adopted.
2. The reason for introducing a dependency outside the spec's recommended dependency table.

Format of each entry: number, milestone, problem, decision, rationale, impact. Consensus freeze of 2026-10-05 (D-096): all entries originally marked "pending confirmation" have been closed out, with their headings changed to "confirmed 2026-10-05"
or noting the decision that superseded them; entries from now on have no "pending confirmation" status -- when the spec has a gap, the problem and the most conservative implementation are still recorded first,
and matters that need the project owner's decision are recorded in an entry after being clarified with the project owner.

The entries up to D-133 were written before the repository was published, and the files each one names as changed are those of that time. Some are no longer
here: the development log (`PROGRESS`, with the milestones' acceptance details and the close-out ledger `PENDING_REVIEW`), the v1 specification, and the
working conventions file in their old form; the web wallet and block explorer (`web/`, with `docs/WEB_DESIGN.md`) moved to the ksana-web repository, the
project's site and docs (`site/`) to ksana-site and the testnet points (`points/`) to ksana-points. `docs/HISTORY.md` summarizes that history.
Mainnet's block share is left out where they gave it, each such passage marked [not published, D-134].

---

## D-001 Definition of OutPoint and the byte order of BTC hashes (M0, confirmed 2026-10-05, D-096)

- **Problem**: §4.1 `FeeMode::BtcMerged { funding: OutPoint, .. }` uses `OutPoint`, which the spec does not define; the byte order of all BTC transaction ID / block hash fields (`OutPoint.txid`, `BtcAnchor.hash`, `Entry::Forced.btc_txid`, `Debt.conflict_txid`, `Unlock.settlement_txid`) is not specified either.
- **Decision**: `pub struct OutPoint { pub txid: [u8; 32], pub vout: u32 }` (36 bytes in total in Borsh, the same as the outpoint field in a Bitcoin transaction input). All BTC hashes are stored in **Bitcoin internal byte order**, i.e. the raw output of sha256d, the byte order that appears in transaction serialization; the hex shown by RPC and block explorers is its reverse.
- **Rationale**: consistent with Bitcoin transaction serialization, no conversion is needed when building / checking settlement transactions, and it is the least error-prone; `vout` as u32 matches Bitcoin.
- **Impact**: `ksana-primitives` types and test vectors; the TS SDK must reverse a txid obtained from RPC before filling it in.

## D-002 Definition of Contribution (M0, superseded: path A deleted, D-078)

- **Problem**: §6.1 `Entry::Merged { op, contribution: Contribution }` is annotated "contains the signed input and the paired output", but the spec does not define the fields of `Contribution`.
- **Decision**:

  ```rust
  pub struct Contribution {
      pub sequence: u32,          // nSequence of the funding input
      pub witness: Vec<Vec<u8>>,  // witness stack of the funding input (SIGHASH_SINGLE|ANYONECANPAY signature)
      pub value: u64,             // amount of the paired output (sats)
      pub script_pubkey: Vec<u8>, // scriptPubKey of the paired output
  }
  ```

  The outpoint of the funding input is the `funding` in `op.fee` and is not stored twice; §5.1 only supports P2TR key-path and P2WPKH, whose scriptSig is always empty, so no scriptSig is stored.
- **Rationale**: these four fields are exactly enough to put the input/output pair back into the settlement transaction as is (including shifting them as a whole in a rescue settlement, §7.3); not duplicating the outpoint avoids the extra invalid case of "the two copies disagree". The amount and scriptPubKey of the funding UTXO are BTC on-chain data; both the node and the indexer read and check them from bitcoind (txindex), so they are not put into the entry.
- **Impact**: entry encoding, the settlement payload (see D-003), settlement construction in M3.

## D-003 Definition of EntryData (M0, confirmed 2026-10-05, D-096; entries in payloads now use the compact encoding, D-080)

- **Problem**: §7.1 `Payload::Settlement { entries: Vec<EntryData> }` is annotated "all entries of this block; Forced-inclusion entries only write btc_txid", but the spec does not define `EntryData`.
- **Decision**: the variants of `EntryData` correspond one-to-one to those of `Entry`, with the same variant indices; `Forced { btc_txid }` writes only the transaction ID, and the fields of the other variants are identical to `Entry`, so except for `Forced` the Borsh encodings of the two are byte-for-byte identical. `Merged` still carries the full `Contribution`.
- **Rationale**: closest to the spec text (only Forced is specially omitted). Keeping `Contribution` in `Merged` is necessary: a rescue settlement drops the input/output pairs that became invalid (§7.3), but the payload must match the block content byte for byte (§7.2), and for an account with fee debt "the part of the contribution exceeding the declared value is automatically offset against the fee debt" (§8.4) needs the amount of the paired output, which the indexer can only get from the payload for a pair that was dropped.
- **Impact**: settlement payload size (about 100–140 more bytes per path A entry, counted toward `MAX_BLOCK_DATA_BYTES`).

## D-004 Merkle construction of entries_root (M0, confirmed 2026-10-05, D-096)

- **Problem**: §6.1 says `entries_root` is "the Merkle root of all entries of this block", but the spec does not specify the construction of the tree (leaf content, leaf / internal node hashing, handling of an odd number of nodes, the empty tree).
- **Decision**: use the Merkle Tree Hash of RFC 6962 (Certificate Transparency), with SHA-256 as the hash function:
  - leaf data `d_i = borsh(Entry_i)`, in the order of the entries in the block;
  - `MTH({}) = sha256("")`; `MTH({d}) = sha256(0x00 ‖ d)`;
  - `MTH(D[0..n]) = sha256(0x01 ‖ MTH(D[0..k]) ‖ MTH(D[k..n]))`, where `k` is the largest power of 2 less than `n`.
- **Rationale**: leaves and internal nodes have domain-separation prefixes and odd nodes are not duplicated, which avoids the CVE-2012-2459 style malleability of Bitcoin's Merkle tree; it is uniquely defined for any `n` (including 0). A leaf is the full `Entry` (Forced including the VmOp); the indexer can recompute it once it has derived the Forced-inclusion entries from BTC per §7.4.
- **Impact**: `ksana-crypto::merkle`, block production and block validation in M2, the indexer in M5.

## D-005 Length limit for payload decoding (M0, confirmed 2026-10-05, D-096)

- **Problem**: §16 requires checking a length limit before decoding. `MAX_BLOCK_DATA_BYTES` is "the total amount of data a single block writes into the settlement transaction", but the `NativeOp` and `Headers` payloads have no limit of their own.
- **Decision**: all three kinds of `Payload` uniformly use `MAX_BLOCK_DATA_BYTES` (80 KiB on devnet) as the decoding limit; `VmOp` uses `MAX_OP_BYTES` (64 KiB) as its decoding limit.
- **Rationale**: this is the only data-size limit the spec defines, and it is below the roughly 100 kvB limit on standard BTC transactions; a `NativeOp` payload is at most 1 + 64 KiB, already within that limit.
- **Impact**: a `Headers` republish transaction carries at most about 417 block headers at a time (80 KiB / 196 B).

## D-006 Additional protocol parameters (M0)

- **Problem**: the "median of the previous 11 blocks" and "no more than 15 seconds ahead of local time" in the body of §6.3 are not in the §13 parameter table.
- **Decision**: following the requirement that "all parameters are defined centrally in `ksana-primitives::params`", add `Params::median_time_span = 11` and `Params::max_future_block_time_secs = 15`; the values of the parameters in the §13 table are copied verbatim. Also define `VMOP_VERSION = 1` (§4.1) and `KSA_ASSET_ID = [0; 32]` (§4.4) at the crate root.
- **Impact**: no protocol change.

## D-007 JSON representation of types (M0)

- **Problem**: the shared test vectors (§11.3) and RPC need a language-independent JSON representation, which the spec does not specify.
- **Decision**: byte strings as lowercase hex; `u64` / `u128` as decimal strings (only the canonical form is accepted); `u8` / `u32` as JSON numbers; enums in serde's default externally tagged form; unknown fields are rejected.
- **Rationale**: JavaScript numbers only have 53 bits of precision; the externally tagged form is easy to construct on the TS side. JSON takes no part in consensus.

## D-008 Dependencies outside the spec's recommended table (M0)

| Dependency | Purpose | Reason |
| --- | --- | --- |
| `sha2` 0.10 | SHA-256 (op_id, block hash, tagged hash, Merkle) | The spec requires sha256 / sha256d; `jmt` from the recommended table itself depends on `sha2` 0.10, so the same implementation is reused |
| `serde`, `serde_json` | JSON representation for test vectors and (later) RPC | `jsonrpsee` itself is based on serde |
| `hex` | Hex encoding / decoding | Test vectors and JSON |
| `thiserror` | Error types | Consensus code forbids panics, errors must be returned; reduces boilerplate |
| Docker image `bitcoin/bitcoin:28.4` | regtest bitcoind for devnet | The spec requires Bitcoin Core ≥ 28.0; this image contains the officially released binaries |
| `wasmparser` 0.258 (M1) | Static checks at deployment in `ksana-vm`: validator with the float feature disabled, instruction-by-instruction scan, call graph and stack estimation | The parser wasmtime itself uses, with the same feature definitions; the static checks need to visit each instruction |
| `anyhow` 1.0 (M1, locked to 1.0.104) | `ksana-state` implements jmt's `TreeReader` | The `TreeReader` methods of jmt 0.12 return `anyhow::Result` and jmt does not re-export anyhow; used only in `ksana-state/src/tree.rs`, the public API returns `StateError`; it was already in Cargo.lock (a dependency of jmt) |
| `wat` 1.258 (M1, tests only) | Writing contracts as WAT text in tests | Precisely constructs float, recursion, trap, gas-boundary and similar test cases |
| `tempfile` 3 (M1, tests only) | Temporary RocksDB directories for tests | — |

`secp256k1` is pinned to 0.29.x: `bitcoin` 0.32 from the recommended table depends on `secp256k1` 0.29, which keeps a single copy of libsecp256k1 in the dependency tree.

`bitcoin` resolves to 0.32.102: rust-bitcoin maintains both the 0.32.1xx line (a maintenance line with a raised MSRV) and the 0.32.x line, `^0.32` picks the highest, 0.32.102, by semver, and Cargo.lock pins that version (it satisfies the recommended table's "0.32 or newer" and includes P2A support).

## D-009 Bip322Simple signatures before M6 (M0)

- **Problem**: §4.1 defines `SigScheme::Bip322Simple` but notes "implemented in M6"; the spec does not say how such a signature received during M0–M5 is handled.
- **Decision**: before M6, `verify_vmop` always returns `SigError::Bip322Unsupported` for `Bip322Simple`, i.e. signature verification fails (fast-path entries therefore cannot be included; path C entries are treated as failing signature verification).
- **Rationale**: most conservative -- an unimplemented verification must never count as passing.
- **Impact**: this branch is replaced once M6 implements BIP322 simple verification.

## D-010 bitcoind version and configuration for devnet (M0)

- **Problem**: §12 only requires Bitcoin Core ≥ 28.0 with txindex enabled, and §7.1 requires regtest to be started with `-datacarriersize=100000 -txindex=1`; the exact version and the rest of the configuration are not specified.
- **Decision**: `devnet/docker-compose.yml` uses `bitcoin/bitcoin:28.4` (pinned by image digest) with the arguments `-regtest -txindex=1 -datacarriersize=100000 -acceptnonstdtxn=0`, RPC credentials `ksana/ksana`, and the port mapped only to 127.0.0.1.
- **Rationale**: 28.x is the lowest major version that satisfies the spec and has the strictest OP_RETURN policy (a single OP_RETURN, 83-byte limit by default), so a transaction that passes on it also passes on newer versions; an explicit `-acceptnonstdtxn=0` ensures devnet accepts only standard transactions, like mainnet.
- **Measured on 28.4 regtest**: without the configuration, an OP_RETURN of >83 bytes is rejected (`scriptpubkey`); with it, a KSN1 output with multiple data chunks and a payload of exactly `MAX_BLOCK_DATA_BYTES` (82,400-byte script, vsize 82,535) are both standard transactions; two OP_RETURN outputs are rejected (`multi-op-return`). The last two are continuously verified by the integration tests.
- **Impact**: `two_op_return_outputs_are_nonstandard` will fail when upgrading to v30+; the relay policy then needs to be re-confirmed per §7.1.

## D-011 Organization of the integration tests (M0)

- **Problem**: §0 requires every milestone to have integration tests that run on bitcoind regtest, while `cargo test` should not depend on external services.
- **Decision**: the integration tests live in the workspace member `tests/integration` (package name `ksana-integration-tests`) but not in `default-members`; they are run with `tests/integration/run.sh`: `docker compose up --wait` → `cargo test -p ksana-integration-tests` → `docker compose down -v`. In CI a separate job runs them; clippy covers them with `--workspace`.
- **Rationale**: unit / property tests keep zero external dependencies; the integration tests run against exactly the same bitcoind configuration as devnet and are never skipped.

---

> The M0 entries pending confirmation (D-001 to D-005, D-009): the project owner instructed "if there are no doubts, continue", so they proceed with the default solutions above; formally confirmed on 2026-10-05 with the consensus freeze (D-096).

## D-012 Contract ABI and host function signatures (M1, confirmed 2026-10-05, D-096; the set of host functions follows D-073)

- **Problem**: §9.2 only requires that "a single entry function receives Borsh-encoded (method, args) and returns a Borsh-encoded result", and §9.3 only lists the host function names.
  The export name of the entry point, the memory allocation scheme, the representation of return values and reverts, and the parameters of each host function are all unspecified.
- **Decision**: see `docs/ABI.md`. Key points: export `memory`, `ksana_alloc(len) -> ptr` and `ksana_call(ptr, len) -> i64`;
  the input is Borsh `(String, Vec<u8>)` and the output is Borsh `Result<Vec<u8>, Vec<u8>>` (`Err` is an explicit revert);
  variable-length return values are allocated by the VM calling back into the contract's `ksana_alloc`; `init` means calling the entry point with the method name `"init"` at deployment.
- **Rationale**: no host functions beyond the spec's table are needed; only i32/i64 are used, so any language that compiles to wasm can implement it.
- **Impact**: `ksana-vm`, `ksana-contract-sdk`, read-only calls in the TS SDK (the `ksana_call` RPC).

## D-013 Values of the gas table and VM limits (M1, confirmed 2026-10-05, D-096; calibration on real hardware not done, adjustments go through the upgrade schedule of D-076)

- **Problem**: §8.3 requires host functions to be charged by a fixed table that is a consensus parameter, but gives no values; the fixed costs of built-in actions and the length limits of storage keys and values are not specified either.
- **Decision**: the values are in `docs/ABI.md` sections 3 and 4 and are defined in `ksana-vm::gas` and `ksana-vm::limits`.
  `gas_used` in the receipt of a failed op is recorded as `max_gas` (receipts do not enter the state root, decoupling it from the details of wasmtime's fuel accounting on trap).
- **Rationale**: the values are set at the orders of magnitude "memory access ≈ 1, storage read ≈ thousands, storage write ≈ tens of thousands, signature verification ≈ 50,000", guaranteeing that under `BLOCK_GAS_LIMIT` (100 million)
  the number of storage writes in a single block is bounded.

## D-014 Deterministic stack limit (M1, superseded by the runtime stack metering of D-073; `STACK_BUDGET` 256 KiB confirmed 2026-10-05, D-096)

> **Rewritten by D-073 (M8.4)**: deployment no longer requires the call graph to be acyclic and no longer statically checks the stack usage of call chains; the stack frame estimation formula and `STACK_BUDGET` are kept,
> but it is now metered deterministically at runtime through instrumentation before compilation. Below is the original M1 decision.

- **Problem**: §9.1 requires deterministic execution, but where a wasm stack overflow happens depends on each architecture's native stack frame sizes, so the same contract may overflow on one of x86_64 and aarch64
  and not on the other, which the spec does not address.
- **Decision**: static checks at deployment: the call graph (with a conservative expansion of `call_indirect`) must be acyclic, and the estimated stack usage of any call chain must not exceed 256 KiB;
  at runtime `max_wasm_stack = 32 MiB`, and blocks execute on a dedicated thread with a 64 MiB stack. See `docs/ABI.md` section 5 for details.
- **Rationale**: an accepted contract cannot trigger a stack overflow on any architecture, so stack overflow cannot become a source of non-determinism; the price is that recursion is forbidden.
- **Impact**: contracts cannot use recursion (including indirect recursion); `ksana-contract-sdk` and the example contracts obey this restriction.

## D-015 Encoding of code_hash, contract_id and asset IDs (M1, confirmed 2026-10-05, D-096)

- **Problem**: §9.2 `contract_id = sha256(deployer ‖ nonce ‖ code_hash)` does not specify the byte encoding of the nonce or the definition of code_hash; the ID of an asset issued by a contract is not specified.
- **Decision**: `code_hash = sha256(code)`; the nonce is the u64 as 8 little-endian bytes (consistent with Borsh); for a contract asset, `asset_id = sha256(issuing contract ID ‖ sub_id)`,
  where `sub_id` is 32 bytes chosen by the contract, so that one contract can issue several assets.

## D-016 Transfer permission for built-in assets (M1, confirmed 2026-10-05, D-096)

- **Problem**: `asset_transfer` in §9.3 does not specify "whose account it transfers from". If a contract were allowed to transfer the caller's assets, a user calling any contract could have all their assets drained.
- **Decision**: `asset_transfer` can only transfer from **the current contract's own account**. A user account can only be debited by an `Action::AssetTransfer` signed by the user.
  Fungible tokens between contracts (the example token contract) use the allowance pattern: `approve` + `transfer_from`.
- **Rationale**: least privilege. The example AMM is built on the token contract's `transfer_from` and needs no larger permission.

## D-017 Pending record for path B token fees (M1, superseded: since D-079 the sponsored fee is recorded by acceptance position and there is no pending record)

- **Problem (spec contradiction)**: §8.5 / §10.1 state that path B token fees are recorded under the key `0x08 ‖ block_hash`. But execution happens before the block header is complete:
  the block hash covers `state_root`, and `state_root` is not known until execution finishes, so the block's own hash is unavailable during execution, a circular dependency.
- **Decision**: while executing block N, accumulate the sponsored fees into the pending record `0x08 ‖ 32 zero bytes` (`LockedFee { producer: N.producer, .. }`);
  at the start of executing block N+1, if the pending record exists, move it to `0x08 ‖ hash(N)` (N is the parent block, whose hash is known) and delete the pending record.
- **Rationale**: the key `0x08 ‖ block_hash` and the fields defined by the spec stay unchanged, only the write is delayed by one block; unlocking has to wait at least for the settlement after `SETTLEMENT_DEPTH` blocks,
  so this one-block delay affects no rule. An all-zero hash cannot be a real block hash.
- **Impact**: `ksana-execution`; an RPC query for "token fees pending unlock" must also count the pending record.

## D-018 The nonce when a path C execution fails (M1; revised 2026-10-02 to the common practice per the project owner's opinion)

- **Problem (spec contradiction)**: §8.1 says "on execution failure (exceeding max_gas, a contract trap or an explicit revert), the fee is deducted as usual and the nonce is incremented as usual";
  §7.4 and §4.3 say of path C "on nonce mismatch, unpaid fee debt or execution failure, the op is recorded as failed and the state is not changed".
- **Decision**: path C follows the specific rule of §7.4: no failure changes the state (the nonce is not incremented). Paths A and B follow §8.1: on execution failure the nonce is incremented and the fee is still deducted.
- **Rationale**: §7.4 is the specific rule for path C and takes precedence over the general rule of §8.1 (path C pays the producer nothing anyway).
- **Risk**: until the user uses up that nonce, a failed native op can be put into a new BTC transaction by anyone, resubmitted, and executed successfully after the state has changed.
  If the owner thinks it should be incremented, only one branch in the execution layer needs changing.
- **Revision (2026-10-02, the project owner: "path C failing without bumping the nonce: what is the usual way, or how do other networks do it?" "go by what a normal coder, thinking normally, would do")**: changed to the common practice of other networks (Ethereum, Cosmos SDK and Aptos all do the same: a transaction that fails authentication -- bad signature or nonce -- does not change
  the state; an authenticated transaction with the correct nonce always consumes the nonce once included, and an execution failure only rolls back the execution itself), consistent with this project's paths A and B:
  - any one of the version, chain ID, encoded length, fee mode `BtcNative`, signature or nonce is wrong: recorded as a failed op, the state is not changed (same as before; an op with the wrong fee mode
    was not signed by the user for path C and cannot be used to consume the user's nonce).
  - all correct: the nonce is incremented and kept; after that, unpaid fee debt, `max_gas` exceeding `NATIVE_MAX_GAS` or a failed action are only recorded as a failed op, rolling back the action itself.
    The `max_gas` check therefore moves after the nonce (the check order changes; the result differs only in the nonce).
  - Effect: a failed native op is public and can no longer be resubmitted by someone else and executed after the state changes (for example once the user later has a balance, has cleared the fee debt, or an upgrade has raised
    `NATIVE_MAX_GAS`); when parallel blocks in the DAG both include the same native transaction, the second copy fails on nonce mismatch and is not executed again.
  - Spec: the general rule of §8.1 (the nonce is incremented as usual on execution failure) now also applies to path C; §4.3's "when the nonce of a path C entry does not match, it is recorded as a failed op
    and the state is not changed" is unchanged; §7.4's "on unpaid fee debt or execution failure ... the state is not changed" becomes: only the nonce is consumed.
  - Implementation: `forced_prepare` in `executor.rs` returns `ForcedPrepared::{Rejected, Consumed, Ready}`; the latter two commit the nonce write first. Tests:
    `forced_failures_consume_the_nonce_only_when_authenticated` in `rules.rs` (including that a failed native op resubmitted after the balance suffices is still not executed),
    `forced_is_blocked_by_fee_debt`, and the nonces in `debt.rs` and `upgrade.rs` are adjusted accordingly; failed path C ops in the determinism test scenario also consume the nonce,
    and the golden roots are updated. This is a consensus change (not yet on mainnet; devnet and signet change directly, without going through the D-076 upgrade schedule).

## D-019 Encoding of state values (M1)

- **Problem**: §10.1 only lists what each prefix holds and does not specify the encoding of the values.
- **Decision**: see `docs/ABI.md` section 7; everything is Borsh. A key whose balance is 0 is deleted rather than written as 0, so that the same state has exactly one representation.
- **Rationale**: the state root is determined only by the set of key-value pairs; only a canonical representation guarantees that different nodes agree on `state_root`.

## D-020 The gas limit must be exact (M1)

- **Problem**: §8.3 states 1 gas = 1 wasmtime fuel, and §8.1 / §9.2 require an op that "exceeds max_gas" to fail and roll back. Measurement (wasmtime 49 source and tests) shows
  that wasmtime checks fuel only at function entry, loop headers and after variable-length bulk operations: the straight-line code after the last check and the function return are not checked again, and consumption beyond the injected amount
  is saturated to 0 in `Store::get_fuel`. As a result a call that consumes slightly more than `max_gas` "succeeds", with `gas_used` truncated to `max_gas`.
  This is deterministic (the check locations depend only on the wasm structure), but it violates "exceeding max_gas means failure".
- **Decision**: the VM injects `max_gas + 1` fuel into wasmtime (1 guard unit). When the call returns, the remaining fuel must be at least 1, otherwise it is judged out of gas;
  host functions likewise deduct the guard unit first when charging. Thus "success" holds if and only if consumption ≤ `max_gas`, and any overrun hidden by saturation is judged a failure.
- **Rationale**: wasmtime is not modified, the change is in one place, and the result is exact. The test `ksana-vm/tests/exec.rs::gas_limit_is_exact_even_after_the_last_fuel_check`
  covers the case "the overrun happens after the last check" (the test fails when the guard unit is removed).

## D-021 Contracts must not link `core::fmt` (M1)

> **Canceled by D-073 (M8.4)**: now that stack depth is metered at runtime, `core::fmt`, recursion and trait objects can all be used; compiling only as `cdylib` is still recommended
> (smaller modules). Below is the original M1 decision.

- **Problem**: D-014 requires an acyclic call graph, and `call_indirect` is conservatively treated as able to call every function of the same type. Rust's formatting machinery (`core::fmt::write` calling
  functions of its own type indirectly through the `dyn fmt::Write` vtable) necessarily forms a cycle in such a conservative call graph, so any Rust contract that links `core::fmt`
  (`format!`, `to_string()`, `panic!` / `expect` with arguments, and the error formatting when borsh deserializes `String` / `bool` / `Option` / enums)
  is rejected at deployment.
- **Decision**: keep the conservative rule of D-014 unchanged. `ksana-contract-sdk` provides decoding (`Decode`) that is fully consistent with Borsh and does not involve `core::fmt`,
  and its panic handler traps directly; release builds of contracts compile only as `cdylib` (when an `rlib` is produced at the same time, LTO cannot internalize symbols and `alloc`'s out-of-memory handler
  pulls in `core::fmt`). The SDK's crate documentation and `docs/ABI.md` section 5 state these constraints, and `ksana-test-contracts/tests/wasm_sanity.rs`
  performs the same check on the three example contracts.
- **Follow-up**: if arbitrary Rust code needs to be supported, this can be changed to a more precise indirect-call analysis (for example reachability by table contents and types), which is a consensus change.

## D-022 The unsafe rule for `ksana-contract-sdk` (M1)

- **Problem**: the workspace sets `unsafe_code = "forbid"`, but on wasm32 calling host functions (FFI), implementing the global allocator and exporting `ksana_alloc` require unsafe,
  and forbid cannot be relaxed at module level.
- **Decision**: only `ksana-contract-sdk` switches to `deny`, with an explicit `allow` only on the wasm32-specific module `src/wasm.rs` and the export functions generated by `entry!`.
  All other crates (including all consensus code) stay at `forbid`.

## D-023 Beneficiary binding in the example contract btc-payment-trigger (M1)

- **Problem**: §9.4 / §9.5 only ask for a "BTC payment triggers a payout" example. If the caller could name any beneficiary when claiming, anyone who sees the payment (including the producer)
  could front-run and claim it with themselves as the beneficiary.
- **Decision**: the payment transaction must have another output whose locking script is exactly `OP_RETURN <32-byte beneficiary>` (`0x6a 0x20 ‖ beneficiary`);
  each `(txid, vout)` can be claimed only once; the block header is read through `btc_header` (depth not less than `BTC_QUERY_DEPTH`). This is application-level design of the example contract,
  not a protocol rule.

## D-024 State tree root and local storage semantics (M1)

- **Problem**: §10 only specifies using `jmt` with RocksDB and that `state_root` is the tree root; the details of key hashing, versions and rollback are not specified.
- **Decision**:
  - key hash = `sha256(full state key)` (the full key already carries the §10.1 prefix byte); leaf and internal node hashes follow the definitions of jmt 0.12
    (the `ksana-state` crate documentation lists the formulas, and `tests/golden.rs` has golden roots computed by an independent implementation). The root depends only on the final set of key-value pairs,
    not on write order or history. **The jmt version, like wasmtime's, is a consensus parameter.**
  - version number = fast-lane height; genesis can be any version (except `u64::MAX`, which jmt reserves). Deletions of nonexistent keys and writes of an unchanged value are filtered out before commit.
  - A rollback deletes everything after the target version; after pruning, only versions `>= keep_from` are readable. A fixed-version read view returns
    `StaleView` once that version has been rolled back or pruned, and never reads data from another branch.
- **Rationale**: these are all local storage semantics and, apart from the definition of the root, do not affect consensus; the root's definition is fixed at jmt 0.12, and golden vectors from an independent implementation guard against silent changes on upgrade.

## D-025 Numeric conventions for PoW (M2, confirmed 2026-10-05, D-096; mining algorithm per D-094)

- **Problem**: §6 only says "SHA256d PoW" and "`bits` is the difficulty target"; it does not specify how the hash is compared with the target, the encoding of `bits`, the PoW limit, or the algorithm for chain work.
- **Decision** (all following Bitcoin's conventions):
  - PoW value: the 32 bytes of the block hash `sha256d(borsh(header))` are interpreted as a 256-bit unsigned integer in **little-endian** order; the block is valid when `value ≤ target`.
  - `bits` is Bitcoin's compact encoding: exponent = `bits >> 24`, mantissa = `bits & 0x007f_ffff`, target = mantissa × 256^(exponent−3).
    A set sign bit (`0x0080_0000`), a target of 0, a target ≥ 2^256, or a target above the PoW limit are all invalid. Converting a target to compact uses the canonical form of Bitcoin's `GetCompact`,
    so a block's `bits` must be bit-for-bit identical to the value computed by the difficulty adjustment.
  - PoW limit (minimum difficulty): `0x207f_ffff` on devnet (the same as BTC regtest, about 2^255), recorded as `Params::pow_limit_bits`; it is also the genesis block's `bits`.
    It is set this low so that devnet produces blocks at 5-second intervals on ordinary CPUs.
  - Work per block = ⌊2^256 / (target + 1)⌋; chain work is the sum over the blocks; fork choice takes the chain with the largest cumulative work, and on a tie keeps the chain received first (local policy).
- **Impact**: `ksana-consensus`.

## D-026 ASERT anchor and integer implementation (M2, confirmed 2026-10-05, D-096)

- **Problem**: §6.3 requires per-block, integer-implemented ASERT (aserti3-2d), but specifies neither the anchor block nor the details of the integer algorithm; the aserti3-2d reference implementation
  also assumes a PoW limit below 2^224, while devnet's limit is about 2^255 (D-025).
- **Decision**:
  - The anchor is the genesis block: anchor height 0, anchor target = the genesis block's `bits`, "anchor parent time" = genesis timestamp − `TARGET_BLOCK_TIME`
    (the exponent is 0 when blocks arrive exactly on schedule).
  - With a parent of height `h` and timestamp `t`, the child's target is computed per aserti3-2d:
    `exponent = ((t − anchor parent time) − T × (h + 1)) × 65536 / τ` (computed exactly, truncated toward zero, matching the C++ division of the reference implementation;
    at extreme heights the product can exceed i128, so the implementation caps the exponent at ±2^40 -- the result is already clamped once |exponent| ≥ 2^24, so the cap does not change the result);
    `shifts = exponent >> 16` (rounded down), `frac = exponent − shifts × 65536`;
    `factor = 65536 + ((195766423245049·frac + 971821376·frac² + 5127·frac³ + 2^47) >> 48)`;
    `next = ⌊anchor target × factor × 2^(shifts − 16)⌋`, computed exactly with sufficiently wide integers; a result of 0 becomes 1, and a result above the PoW limit becomes the limit.
    T = `TARGET_BLOCK_TIME`, τ = `ASERT_HALF_LIFE`. Under the premise of the reference implementation (limit < 2^224) the results are identical to the reference implementation.
- **Rationale**: using the genesis block as the anchor is simplest and needs no extra activation height; wide integers only extend the usable range without changing the formula.
- **Verification**: `ksana-consensus` checks bit-for-bit against the official Bitcoin Cash Node aserti3-2d test vectors (`run01`–`run12`, 14,000 in total, BCH parameters),
  and an independent arbitrary-precision Python reference implementation (`tests/data/asert_ref.py`) generates vectors for the devnet parameters and edge cases; convergence is covered by simulation tests at hashrates of 1e4 to 1e9 hashes/s
  and with sudden hashrate changes.
- **Note**: unlike BCHN, the "anchor parent time" here is the genesis timestamp − T (BCHN uses the genesis block's own time when it anchors at the genesis block), so BCHN's code cannot be copied directly.

## D-027 Block header version and the genesis block (M2, confirmed 2026-10-05, D-096; version number per D-063 / D-076, for the mainnet genesis config template see D-096)

- **Problem**: §6.1 gives neither the value of the block header's `version` nor the contents of the genesis block.
- **Decision**: `version = 1` (`ksana_primitives::BLOCK_VERSION`); other values are invalid. Genesis block: height 0, `parent` all zeros, `btc_anchor` from the network config,
  no entries (`entries_root` is the root of the empty entry list), `state_root` is the root of the genesis state (the genesis allocation of D-019), `producer` all zeros,
  timestamp from the network config, `bits` defaulting to `pow_limit_bits`, `nonce = 0`; PoW is not checked on the genesis block. The devnet genesis config (timestamp, BTC anchor, genesis allocation,
  and an optional genesis `bits`) comes from the node's config file (`ksana-node --genesis`, format in `crates/ksana-node/src/genesis.rs`);
  all nodes of the same devnet must use the same config; the data directory records the config fingerprint, and opening it with a different config is refused.
- **Rationale for a configurable genesis `bits`**: ASERT's convergence time constant is about the half-life (600 seconds). Starting from a difficulty much easier than the equilibrium, each block already takes several seconds during the last few halvings,
  and converging to one block per 5 seconds takes tens of minutes; if devnet is given a genesis difficulty close to the equilibrium (slightly easier) for its actual hashrate, it produces blocks at about 5 seconds right away. It can only be harder than the PoW limit,
  and it is ASERT's anchor target (D-026).

## D-028 BTC anchoring and the settlement transaction stub in M2 (M2)

- **Problem**: M2 requires a single node to produce blocks and run path B end to end, while BTC header tracking, the anchoring rules, and the funding, signing and broadcasting of settlement transactions are implemented in M3 (§14: "in this phase the settlement transaction can start as a stub (constructed only, not broadcast)").
- **Decision**:
  - Before M3, every block's `btc_anchor` equals the genesis block's anchor, and node validation requires equality; no BTC block header is readable during execution (`btc_header` fails).
  - A block containing fast-path entries carries a "settlement transaction" as usual, but it is a stub: v2, no inputs, and the outputs are, in order, an OP_RETURN data output (the full `Payload::Settlement`) and
    a 240-sat P2A anchor output (the P2A dust limit of Bitcoin Core 28); it is not funded, signed or broadcast. Block validation only checks the presence of the settlement transaction and the payload contents
    (the payload must match the block contents byte for byte and not exceed `MAX_BLOCK_DATA_BYTES`); the remaining rules of §7.2 are added in M3.
  - Block headers in the settlement payload: from just after the previous block with a settlement transaction up to and including this block, not counting the genesis block (it comes from the network config), at most the most recent `MAX_SETTLEMENT_HEADERS` of them.
    When producing a block, payload space is reserved for this many headers, and entries are included according to the remaining space.
- **Impact**: `ksana-node` (`settlement.rs`, `chain.rs`). M3 replaces it with real settlement transactions.

## D-029 Format and semantics of the node RPC (M2)

- **Problem**: §11.2 only lists method names and purposes, without specifying the format of parameters and results.
- **Decision**: see `docs/RPC.md`. Key points: a VmOp is submitted as Borsh-encoded hex (test vectors shared with the TS SDK, byte-for-byte identical); the JSON conventions are the same as in `tests/vectors/`
  (hex, decimal strings); `ksana_getBlock` additionally accepts `"latest"` (the spec provides no method to query the chain tip); when this node produces blocks, `ksana_submitOp`
  returns only after the new op has entered the block template, guaranteeing "takes effect in the next block"; the pending sponsored-fee record of the latest block (D-017) is listed in `ksana_getAccount` under the latest block's hash.
  `ksana_estimateFee` and `ksana_getSettlement` depend on BTC and are added in M3.

## D-030 Reception time rules and this node's block production (M2)

- **Problem**: the timestamp rules of §6.3 (greater than the median of the previous 11 blocks, at most 15 seconds ahead of local time) are checked only when receiving new blocks, not when syncing historical blocks; the spec does not say how the producer picks the timestamp.
- **Decision**: when producing a block, take `max(local time, median of previous 11 blocks + 1)`; blocks produced by this node skip the reception checks. From M4, new blocks received from the network are checked per §6.3 (the `now` parameter of `Chain::import_block`),
  and `None` is passed to skip it when syncing historical blocks.

## D-031 The example contracts' wasm is not byte-for-byte reproducible across checkout paths (M1)

- **Problem**: on CI's x86_64 job, the `state_root` of blocks 7–10 of the determinism test (real token / AMM contracts) differs from the golden value, while locally and on the aarch64 job it matches.
  Cause: `contracts/` is a separate workspace that references the SDK via `../crates/ksana-contract-sdk`; the SDK is outside that workspace root, so cargo uses its
  **absolute path** to compute the crate metadata hash (`SourceId::stable_hash` only relativizes paths inside the workspace root), the symbol names change accordingly,
  rustc orders cross-crate functions by symbol name, and the function order in the linked wasm therefore varies with the checkout path (repeated builds at the same path are unchanged).
  Different code → different `code_hash` and contract ID → different state root. This is not execution non-determinism: executing the same bytecode gives the same result on every machine.
- **Decision**: the golden roots of the determinism test depend only on fixed inputs: the real contracts now use fixed bytecode committed to the repository,
  `crates/ksana-execution/tests/data/{token,amm}.wasm` (token `6f9492e0…`, amm `f09f1bcd…`, i.e. the build that produced the existing golden roots;
  after M8 added the attached-funds parameter to `call_contract`, amm was rebuilt as `d4a4d2f3…`, D-073; M8.5 added the fixed bytecode of the test contract probe,
  `ca6b5703…`, and blocks 11 to 13).
  Other tests keep using contracts built on the spot by `ksana-test-contracts` (they do not depend on the exact bytes).
- **Follow-up**: if reproducible contract builds are needed (e.g. so users can verify on-chain code), the workspace root that builds the SDK and the contracts should contain both, or the build should run in a container at a fixed path.
  D-119 does the first for the playground's modules: they are built in a copy of the contracts workspace that reaches the SDK through a link inside its root, and with
  one host line for cargo on every machine, as the architecture of the machine that builds enters the same hash.

## D-032 Parsing rules for KSN1 data outputs (M3, confirmed 2026-10-05, D-096; since D-081 OP_RETURN carries only path C)

- **Problem**: §7.1 only gives `OP_RETURN <magic> <chunk1> <chunk2> …`, without specifying how the pushes are encoded or how a transaction with several KSN1 outputs is handled.
  The M2 stub parsed with the "minimal push" rule, while rust-bitcoin writes a 1-byte data chunk with `OP_PUSHBYTES_1` (for values 1–16 the minimal encoding would be `OP_1`–`OP_16`),
  so when the payload length ≡ 1 (mod 520) and its last byte is 0x01–0x10 or 0x81, a settlement transaction we construct ourselves is judged malformed by ourselves.
- **Decision**:
  - **KSN1 output**: the locking script starts with `OP_RETURN` immediately followed by a push whose data is exactly `magic`.
  - **Data script**: the rest of the KSN1 output may only be data pushes (`OP_PUSHBYTES_0..75`, `OP_PUSHDATA1/2/4`, **non-minimal encodings accepted**), each at most 520 bytes;
    any other opcode (including `OP_1`–`OP_16` and `OP_1NEGATE`) or a truncated push makes it malformed. The payload is the concatenation of the data chunks in order.
  - The payload is taken from a transaction only when it has **exactly one** KSN1 output and that output is a well-formed data script; a transaction with no or more than one KSN1 output is not recognized
    (neither as a native transaction nor as a settlement / republish transaction). The payload's length is checked against `MAX_BLOCK_DATA_BYTES` first and then it is fully decoded as a `Payload` (D-005).
  - When constructing, always write with rust-bitcoin's `push_slice` (push opcode chosen by length), 520 bytes per chunk, with the last chunk allowed to be shorter.
- **Rationale**: every encoding of a data push is a standard `OP_RETURN` for Bitcoin Core (which only requires push-only), so the parser should not be stricter than BTC;
  "exactly one" avoids the ambiguity of which output to take when there are several (Core 28 only relays transactions with a single `OP_RETURN` anyway).
- **Impact**: `ksana-btc::data`; native transaction recognition, settlement transaction validation and the indexer (M5) all use the same implementation.

## D-033 Path A: PSBT submission format and funding input rules (M3, superseded: path A deleted, D-078)

- **Problem**: §5.1 only describes the flow; the exact shape of the PSBT, the transaction fields covered by the signature, the meaning of a funding input "confirmed before the anchor height", and the contents of the `0x07` record are all unspecified.
- **Decision**:
  - **Signature convention**: a `SIGHASH_SINGLE|ANYONECANPAY` signature (BIP143 / BIP341) commits to the transaction's `nVersion` and `nLockTime`, but not to the input index,
    the other inputs or the other outputs. All settlement transactions are therefore fixed at **v2, `nLockTime = 0`**, and the transaction the user signs must also be v2, `nLockTime = 0`.
  - **PSBT** (the `psbt` of `ksana_submitOp`, BIP174 binary as hex): exactly 1 input (the funding input, which must equal the VmOp's `funding`) and 1 output (the paired output);
    the unsigned transaction is v2, `nLockTime = 0`. The witness is taken from `final_script_witness`; when absent it is assembled from `tap_key_sig` (P2TR) or the single `partial_sigs` entry (P2WPKH).
  - **Funding input types**: P2TR key-path (the witness is exactly one 65-byte element: 64-byte signature + `0x83`, no annex allowed) or P2WPKH (the witness is a DER signature + `0x83` and
    a 33-byte compressed public key whose hash equals the witness program; the signature must be strict DER (BIP66, a consensus rule) and low-S (a Bitcoin Core standardness rule,
    otherwise `testmempoolaccept` rejects it). The signature is computed and verified over "the v2, `nLockTime = 0` transaction with only this one input and this one output"
    (equivalent to any position in the settlement transaction). The top bit of `nSequence` must be 1 (BIP68 relative timelock not enabled), otherwise the settlement transaction's validity would depend on when it lands on chain.
  - **Paired output**: the locking script is one of P2PKH, P2SH, P2WPKH, P2WSH or P2TR, and the amount is not below its dust limit (at a dust relay fee of 3 sat/vB,
    matching the Bitcoin Core default).
  - **"Confirmed before the anchor height"**: the transaction containing the funding output is confirmed in a block of height ≤ `anchor.height` on the anchored chain (i.e. at least 1 confirmation as seen from the anchor block).
    Whether it is spent afterwards is not checked -- "currently unspent" is queried from bitcoind only when receiving a new block (§6.3).
  - Contribution = funding output amount − paired output amount, which must be ≥ `contribution_sat`; for an account with fee debt, the excess offsets the debt (§8.4).
  - The checks above on the witness, signature, output shape and `nSequence` belong to settlement transaction validation (`ksana-btc::contribution`, D-034); the execution layer only checks
    the state-related parts (the signing account, the nonce, that the funding output exists with a sufficient amount, and that the funding input has not been used before).
  - **`0x07` record**: `FundingRecord { op_id, account, contribution_sat: u64, value: u64, script_pubkey: Vec<u8>, debt: bool }`
    (`value` / `script_pubkey` are the paired output, `debt` indicates that fee debt has been recorded), written when a `Merged` entry executes and kept even if the action fails.
    A `Merged` entry whose funding input already has a `0x07` record makes the block invalid (the same input cannot fund twice; a duplicate within the block is equally invalid).
    The last three fields serve M5's fee-debt verification (whether the conflicting transaction kept the original paired output, the debt amount, and that the same `op_id` is recorded only once).

## D-034 Structure and validation of settlement transactions (M3, confirmed 2026-10-05, D-096, in the form revised by D-078 and D-081)

- **Problem**: §7.2 gives the positions of inputs and outputs and a few rules, but the type and signing method of the producer's inputs, the shape of the change and data outputs, how the fee is calculated,
  and how "settlement transactions must not spend each other" is determined are all not precisely defined.
- **Decision** (let n be the number of `Merged` entries in the block):
  - v2, `nLockTime = 0`; the serialized length and weight do not exceed 400,000 (the BTC standard transaction limit); decoding checks the length first and requires full consumption.
  - Inputs `0..n`: the funding inputs of the `Merged` entries in order, with empty `scriptSig`, `nSequence` and witness byte-for-byte identical to the `Contribution` in the entry,
    and passing the signature verification of D-033. Outputs `0..n`: the paired outputs of the entries in order.
  - Inputs `n..`: at least one producer input, with empty `scriptSig`, and only P2TR key-path (the witness is a 64-byte signature, or 65 bytes with last byte `0x01`)
    or P2WPKH (a strict-DER, low-S signature with last byte `0x01` + a 33-byte compressed public key) -- i.e. all of them cover all inputs and outputs (a 64-byte P2TR signature is
    `SIGHASH_DEFAULT`, covering the same scope as ALL, but the BIP341 digest commits to the type byte, so the two digests differ; a 65-byte signature with last byte `0x00` is invalid per BIP341),
    and the node verifies the signatures with the previous outputs of all inputs. Their previous outputs must be confirmed in blocks of height ≤ `anchor.height` on the anchored chain.
  - Exactly n + 3 outputs: `n` is the producer's change (same shape and dust rules as the paired output of D-033, and not a KSN1 output), `n+1` is the single KSN1 data output
    (amount 0, payload byte-for-byte identical to the block contents, header selection unchanged from D-028), and `n+2` is a 240-sat P2A output (`OP_1 <0x4e73>`, the dust limit of Bitcoin Core 28).
  - All inputs are distinct; fee = sum of previous output amounts − sum of output amounts ≥ `vsize × MIN_SETTLEMENT_FEERATE`, `vsize = ⌈weight / 4⌉`.
  - The weight limit may bind before the data size limit: a full data output (a script of about 82,400 bytes) takes about 330,000 WU, leaving about 70,000 WU for input/output pairs (about 400 to 450 WU per pair).
    The producer therefore includes at most 150 path A entries per block (local policy), and when assembly fails, the block includes no fast-path entries.
  - **When receiving a new block** (and before the producer submits its own block) additionally check: all inputs are unspent in bitcoind's UTXO set, and `testmempoolaccept` accepts the transaction.
  - **"Settlement transactions must not spend each other"**: guaranteed by "producer inputs must be confirmed before the anchor height" -- a settlement transaction can only spend confirmed outputs,
    so it never forms a chain with another unconfirmed settlement transaction. The producer's change can be used for funding again once the settlement containing it confirms (otherwise the funding UTXOs would keep running out).
    M3's acceptance is understood accordingly: at no time do settlement transactions in the mempool have a parent-child relationship with each other.
  - **Settlements confirmed on BTC** (used for the settled prefix and unlocking, D-036): same structure as above, but the user input/output pairs may be an order-preserving subsequence of the `Merged` entries
    (rescue settlement, §7.3), the funding input types are unrestricted and no longer signature-checked (BTC has already verified them), and previous outputs are looked up via txindex.
- **Rationale**: having all producer inputs use `SIGHASH_ALL` makes the whole transaction (including the data output and change) unmodifiable by third parties; all rules can be decided deterministically from the block contents and the anchored chain,
  so the result is unchanged when syncing historical blocks (when "currently unspent" and the mempool can no longer be queried).

## D-035 BTC anchoring and the execution layer's BTC view (M3, confirmed 2026-10-05, D-096; mainnet `ANCHOR_DEPTH` 2 confirmed 2026-10-06, native transactions after a deep reorg deferred to after mainnet)

- **Problem**: §6.3 requires `btc_anchor` to be on the BTC chain the node sees; executing path A, unlock entries and contracts reading block headers all need "the BTC data at the anchor",
  but the semantics of the data view and the handling of BTC reorgs are not specified.
- **Decision**:
  - Anchoring rule: `btc_anchor` must be on the local bitcoind's best chain (both hash and height match), with a height not below the parent's anchor height; when receiving a new block it is further required that
    local BTC tip height − anchor height ≤ `MAX_ANCHOR_LAG`. An anchor block above the locally synced tip means the local state is behind (retry later) and does not make the block invalid.
    The producer uses the fully scanned local BTC tip as the anchor.
  - `ksana_execution::BtcView` gains two queries besides block headers: `funding_output(outpoint)` (a transaction output confirmed on the anchored chain at height ≤ the anchor height,
    regardless of whether it is spent afterwards) and `settlement_confirmed(block_hash, txid)` (the transaction is a valid settlement of that block, confirmed on the anchored chain at height ≤ the anchor height,
    and per the anchored chain the block has entered the settled prefix, D-036). A data source failure returns `Err`, which the execution layer treats as a "local fault" and never uses to declare the block invalid.
  - The node's implementation of the view answers only while the anchor block is still on bitcoind's best chain; if the anchor block leaves the best chain during a query, it returns a local fault.
  - Accepted fast-lane blocks are not invalidated by later BTC reorgs (§15: "a 1-block BTC reorg leaves the fast-lane state unchanged"); when an included native transaction disappears because of a reorg, a warning is logged (§7.4).
- **Known limitations**: after a BTC reorg, native transactions on the new branch at heights not above `h(parent)` are no longer included (the window of §7.4 only advances by height); when syncing historical blocks,
  a block whose anchor block has been reorged away cannot be validated. Both are spec-level issues, to be reassessed with multi-node sync in M4. (M6.1: the latter is solved by D-056; the former is recorded as a remaining limitation of D-056, and solved by D-104 in M29.)

## D-036 Recognition, gas and data size of forced inclusion (M3, confirmed 2026-10-05, D-096)

- **Problem**: §7.4 requires a block to include all native transactions in the window at its start, while §8.3 requires the block's total gas (summing each entry's `max_gas`) not to exceed `BLOCK_GAS_LIMIT`.
  Native transactions are sent on BTC by anyone, `max_gas` can be arbitrarily large and their number is limited only by BTC block capacity: in extreme cases the two rules contradict each other,
  and the M1 implementation would let a single native transaction with `max_gas = u64::MAX` make it impossible to legally produce any block (the fast lane stalls).
- **Decision**:
  - Recognition (§7.4): the transaction has exactly one KSN1 output (D-032), the payload decodes as `Payload::NativeOp { op }` and `op.fee == BtcNative`; coinbase transactions are covered as well.
    `h(x) = x.anchor.height − NATIVE_CONFIRMATIONS + 1` is computed with signed integers; block B's window is `(h(parent), h(B)]`, ordered by BTC height then index within the block, ascending.
    The block's entries must start with these forced-inclusion entries (both `btc_txid` and `op` identical), and no further `Forced` entries may follow.
  - Gas: forced-inclusion entries count `min(max_gas, NATIVE_MAX_GAS)` toward the block's total gas but are **themselves not bound by `BLOCK_GAS_LIMIT`**;
    fast-path entries (paths A / B) are valid only if the total still does not exceed the limit after adding them; fee-debt and unlock entries count 0 gas and are not bound by the limit.
    When the forced-inclusion entries already exceed the limit, the block may contain no fast-path entries.
  - Data size: a forced-inclusion entry takes only its `btc_txid` (33 bytes) in the settlement payload and counts toward `MAX_BLOCK_DATA_BYTES` together with the other entries; when they do not fit, the block may contain no fast-path entries
    (a block containing only forced-inclusion entries needs no settlement transaction).
- **Rationale**: forced inclusion is the censorship-resistance guarantee and must not be blocked by the gas limit; its execution cost is bounded by BTC fees and block capacity, and an op exceeding `NATIVE_MAX_GAS` simply fails without executing.

## D-037 Broadcast timing of settlement transactions and producer funding (M3)

- **Problem**: §7.2's "broadcast only after the block has `SETTLEMENT_DEPTH` confirmations on the fast lane" does not say how confirmations are counted; how the producer manages its funding UTXOs is unspecified.
- **Decision**:
  - A block at height h is broadcast when the main chain tip height ≥ h + `SETTLEMENT_DEPTH` (there are already `SETTLEMENT_DEPTH` blocks on top of it): consistent with "2 blocks on devnet, about 10 seconds",
    and also with "warn when the reorg depth exceeds `SETTLEMENT_DEPTH`" -- a reorg no deeper than that never involves already-broadcast settlement transactions.
    A settlement transaction that was broadcast but is neither in the mempool nor confirmed is rebroadcast (BTC reorg, bitcoind restart).
  - The producer's funding wallet is separate from its payee account (`header.producer`): the node is configured with one BTC funding private key, and the funding address is its P2TR key-path (BIP86-tweaked) address;
    on every BTC tip change the node fetches the confirmed funding UTXOs with `scantxoutset` (excluding immature coinbase), leaving out inputs already taken by this node's not-yet-confirmed settlement transactions;
    change goes back to the same address. When no UTXO is available, the block contains only forced and unlock entries. The fee rate comes from the node config (default 2 sat/vB, not below `MIN_SETTLEMENT_FEERATE`).
  - Before submitting its own block, the producer runs the same checks on the settlement transaction as on reception (including `testmempoolaccept`); on failure it discards the block and evicts the offending ops.

## D-038 Settled prefix, unlock entries and BTC finality (M3, confirmed 2026-10-05, D-096, in the form of D-077 / D-082; unlock per D-074 / D-079)

- **Problem**: §6.4 / §8.5 / §10.4 define unlocking and BTC finality in terms of the "settled prefix", but give no computable definition and do not describe the execution details of `Unlock` entries.
- **Decision**:
  - The **data landing height** `data(b)` of a main-chain block b at height ≥ 1: for a block with a settlement transaction, the earliest confirmation height on the current BTC main chain of any of its valid settlements (confirmed settlements per D-034);
    for other blocks, the earliest confirmation height at which its header appears in any KSN1 settlement or republish payload (headers are compared by hash; the payload only needs to decode, and that settlement itself need not be valid).
    `prefix(b) = max(prefix(parent), data(b))`, 0 for the genesis block; once any block has no data, it is undefined for everything after it.
    Block b belongs to the settled prefix at BTC height H if and only if `prefix(b) ≤ H`.
  - `Unlock { block_hash, settlement_txid }` is valid if and only if: `block_hash` is not all zeros (the pending record of D-017), `0x08 ‖ block_hash` exists and is not yet unlocked,
    `settlement_txid` is a valid settlement of that block with confirmation height ≤ the anchor height, and `prefix(that block) ≤ anchor height` (`BtcView::settlement_confirmed`).
    Execution: the locked amount is added to the producer's available KSA balance, and the record is kept with `unlocked = true` set (the same block can be unlocked only once). An unlock entry counts 0 gas
    and takes 65 bytes in the settlement payload.
  - In each block's fee-debt and unlock phase, the producer writes unlock entries for all unlockable records in the main-chain state (in ascending block height, at most 100 per block).
  - **M9 revision (D-074)**: unlocking becomes cumulative -- at most one `Unlock` per block, which unlocks from the head of the pending-unlock queue up to the block the entry names, and a record is deleted once unlocked;
    when `settlement_txid` is all zeros, it only requires that the block has entered the settled closure. The producer writes at most one per block, pointing at the last settled block.
  - `btc_final` of `ksana_getOpStatus`: the containing block b satisfies `prefix(b) + BTC_FINALITY_CONFS − 1 ≤ BTC tip height` (all the transactions that brought it into the prefix have enough confirmations).
- **Impact**: `ksana-execution` (`Unlock`), `ksana-node` (BTC scanning, prefix computation, unlock entries, RPC); the indexer (M5) uses the same definition.

## D-039 Updates to D-028 and D-029 (M3)

- The stub implementation of D-028 is replaced by real settlement transactions: `btc_anchor` follows D-035, and settlement transactions are funded and signed per D-034 and broadcast with a delay (D-037).
- In D-029, "`ksana_estimateFee` is added in M3" is corrected to: added in M6 per §14; `ksana_getSettlement` is added in M3 (see `docs/RPC.md`).

## D-040 P2P protocol (M4, confirmed 2026-10-05, D-096; the `Contribution` part became void with the deletion of path A)

- **Problem**: §11.1 only lists the gossipsub topics, the uses of request-response, static bootstrap and scoring; it does not specify message formats, the sync protocol or size limits.
- **Decision** (libp2p 0.57, crate `ksana-p2p`):
  - Transport: TCP (`/dns4` addresses allowed) + Noise + Yamux, plus identify and ping; the node key (ed25519) is stored in the data directory as `p2p.key`;
    devnet uses a static bootstrap list (`--peer <multiaddr>`, the `/p2p/<peer id>` part may be omitted).
  - gossipsub topics `/ft/1/blocks` (Borsh(`Block`), including the signed settlement transaction) and `/ft/1/ops` (Borsh(`OpMessage { op, contribution: Option<Contribution> }`):
    a path A op carries the `Contribution` extracted from the user's PSBT (witness, nSequence, paired output), which the receiver re-checks against the funding UTXO per §5.1).
    §11.1's wording is "path A carries a partially signed PSBT": the `Contribution` is exactly the part of the PSBT the user has signed (the funding input's witness and the paired output),
    so the information is the same; the reasons for using it instead are that the encoding is unique, that each hop need not re-parse the PSBT, and that path A ops put back into the entry pool after a reorg (block entries hold only the
    `Contribution`, not the original PSBT) can still be forwarded. The RPC `ksana_submitOp` still accepts a PSBT.
    Manual validation: after processing, the node reports Accept (valid and new, forwarded on; side-chain blocks whose work does not win out are forwarded too) / Ignore (duplicate, unknown parent, rejected only because of the local view
    (anchor not on the local BTC chain, timestamp, an op invalid under the local state), local fault; not forwarded, no score penalty) / Reject (objectively invalid: invalid block, invalid op format or signature;
    not forwarded and penalized).
  - request-response protocol `/ksana/sync/1` (`/ksana/sync/2` from M7 on, see D-063); requests and responses are both Borsh: `Status` (genesis hash, tip height / hash / cumulative work),
    `GetBlocks { locator, max }` (the locator is the block hashes taken from our own main chain at exponentially spaced intervals; the peer returns at most `max` main-chain blocks after the first hash that is on its main chain,
    `max ≤ 500`), `GetBlock { hash }`, `GetMempool { max }`.
  - Peers with a different genesis hash (`Status` mismatch) are disconnected immediately. Received messages are checked against the length limit before decoding: gossip 16 MiB, request / response 32 MiB (local limits,
    not consensus; the size of a fast-lane block is dominated by the forced-inclusion native transactions and is of the same order as a BTC block). D-132 (2026-10-28) lowers the limit of
    a gossip op to `MAX_OP_BYTES`, of a request to 64 KiB, and of the responses to `Status` and `GetMempool` to 64 KiB and 4 MiB.
  - Scoring: a peer that sends invalid blocks or invalid ops (Reject) accumulates score penalties; past the threshold it is disconnected and refused connections for a period (local policy).
  - Implementation details (local policy, not consensus): the gossip message ID is the content hash, so duplicate publishes and messages forwarded back are deduplicated; sync requests and responses are a 4-byte little-endian length prefix
    + Borsh, with the length limit checked before the body is read; sync proceeds in rounds, and each subsequent round's locator starts with the last block of the previous round, so a side chain longer than 500 blocks can also be continued;
    when a sync accepts no block at all, the node does not sync from that peer again as long as its tip is unchanged (except via `request_sync`); invalid ops in a GetMempool response cost at most 1 point
    (whether an op is valid depends on the local state). Default parameters (`Tuning`): ask connected peers for their Status every 3 seconds, redial unconnected bootstrap peers every 2 seconds,
    disconnect at 3 points and ban for 60 seconds, request timeout 30 seconds.
- **Impact**: `ksana-p2p`, `ksana-node`.

## D-041 Block tree, fork choice and reorg (M4, confirmed 2026-10-05, D-096; fork choice per D-063)

- **Problem**: §6.4 only says "choose the valid chain with the most cumulative work" and "keep state snapshots of the most recent `REORG_WINDOW` blocks for rollback"; it does not specify when side-chain blocks are validated,
  how to recover during a reorg, or the reception rules while a reorg is in progress.
- **Decision**:
  - The block store keeps every block whose header is valid (not only the main chain); the `main` height index and the op index cover only the main chain.
  - Receiving a block: parent unknown → orphan pool (at most 256) and sync from the source peer; parent is the main-chain tip → full validation (reception rules) and attach; parent on a side chain →
    validate the header (version, height, PoW, `bits` computed from its parent, the median timestamp of its ancestors and the local time, BTC anchor) and the settlement transaction (without checking that the inputs are
    "currently unspent"), store it, and reorg when its cumulative work is strictly greater than the main chain's. When an already stored side-chain block is received again and its work is still greater than the main chain's (the earlier reorg was interrupted for local reasons), the reorg is retried.
  - Invalid marking: the block hash covers only the header, **not the settlement transaction**, and the entries are committed only indirectly via `entries_root`. Therefore a hash is marked invalid (together with its descendants) only when a rule of the header itself (version, height,
    parent hash, `bits`, PoW) fails, or when the entries match `entries_root` but the execution result (forced-inclusion prefix, state root) does not;
    entries not matching `entries_root`, a missing or invalid settlement transaction, and failures of rules that depend on the local view, such as the BTC anchor and the timestamp, reject only this copy or this attempt,
    and a correct copy of the same hash can still be accepted later. Among these, rejections due only to reception rules or the local view (anchor not on the local BTC best chain, anchor lag, timestamp,
    settlement transaction inputs currently spent) are classified separately (node error `Rejected`), and the peer that forwarded the block is not penalized (D-040).
  - Fork choice: the most cumulative work; on a tie, the chain received first is kept.
  - Reorg: the state is rolled back to the fork point (depth at most `REORG_WINDOW`), and the blocks of the new branch are fully validated and executed in order; these blocks are handled as in "syncing historical blocks" (`now = None`,
    anchor lag, the timestamp and the settlement transaction's "currently unspent" are no longer checked -- a settlement already on BTC on the competing branch may have spent the same funding input).
    If any block of the new branch is invalid: mark it invalid (together with its descendants), roll back and re-execute the original main chain, restoring the original state.
  - A reorg deeper than `SETTLEMENT_DEPTH` logs a highest-level alert (§6.4). For blocks removed from the main chain: their settlement records are no longer broadcast (records already broadcast raise an alert),
    their path A / B ops are put back into the entry pool if still valid, and their op index entries are deleted.
  - A settlement on BTC may be confirmed before this node knows the corresponding fast-lane block (a late joiner, catching up after downtime, previously isolated): the scan first records it as pending resolution, and after the node has stored that block,
    before each block is attached and after each round of BTC sync, it is re-validated against the block's content and counted; otherwise a legitimate block containing that block's unlock entry would be wrongly judged invalid.
  - Crash recovery: at startup, find the highest version v (within `REORG_WINDOW`) that is not above the latest state version or the main-chain height and whose state root matches the main-chain block, and roll the state back to v;
    if the main chain is above v, rewind the main chain to v, and after syncing with bitcoind re-validate and execute the blocks after v on the original main chain as a "reorg".
- **Rationale**: the state store keeps one version per height, so only one chain's state can exist at a time; a side chain is fully executed only when it becomes the heaviest, and forging a side chain costs an attacker real work.

## D-042 Deployment of the 3-node devnet (M4)

- **Decision**: `devnet/Dockerfile` is a multi-stage build of `ksana-node` and `ksana-keytool` (which generates a private key and prints the x-only public key and the regtest P2TR funding address);
  `devnet/docker-compose.yml` adds `init`, `btc-miner` and 3 nodes (profile `nodes`; by default only bitcoind starts, so the existing integration tests are unaffected),
  and the nodes are each other's static bootstrap peers (`/dns4/<service name>/tcp/9000`). `devnet/init.sh init` generates, after bitcoind starts, the genesis config (anchored at the BTC tip at that moment,
  timestamp taken at that moment, a genesis allocation of KSA to the test account alice) and each node's funding and producer private keys, and sends 4 confirmed 1 BTC UTXOs to each funding address;
  `init.sh miner` mines one BTC block every 10 seconds (regtest has no miners, and settlement transactions and native transactions need confirmations). The genesis difficulty is the PoW lower bound, and the nodes use
  `--min-block-interval-ms 5000` to limit the block rate during the start-up phase, with ASERT gradually adjusting to the actual hashrate.
  The acceptance tests use 3 in-process nodes (easier to control isolation and block production); the devnet is for manual runs and demos.

## D-043 Which blocks must carry a settlement transaction (M5, confirmed 2026-10-05, D-096, in the form of D-077)

- **Problem**: §6.3 and §7 only say "a block containing fast-path entries must carry a settlement transaction" and "empty blocks and blocks containing only forced-inclusion entries are not settled separately". Unlock and fee-debt entries are neither fast-path entries
  nor derivable from BTC; M3 only required blocks with `Merged` / `Sponsored` entries to carry a settlement transaction, so the content of a block containing only unlock (or fee-debt) entries would never land on BTC,
  the indexer could not rebuild those blocks (§10.3), and the settled prefix was undefined for them (D-038 only distinguishes "blocks carrying a settlement transaction" from empty blocks / blocks containing only forced-inclusion entries).
- **Decision**: a block containing any entry other than forced inclusion (path A, path B, fee debt, unlock) must carry a settlement transaction; empty blocks and blocks containing only forced-inclusion entries must not carry one.
  The data landing height of D-038 follows this distinction. The producer writes fee-debt and unlock entries only when it can fund the settlement transaction. The settlement payload format is unchanged.
- **Rationale**: this way the indexer can obtain every block's complete entries from BTC data alone: the entries of a block without a settlement transaction are exactly the forced-inclusion entries derived per §7.4,
  and the entries of the other blocks are in the settlement payload. Relative to M3 this is a tightening of the consensus rules (a block containing only unlock entries without a settlement transaction becomes invalid); there is no deployed chain outside devnet.

## D-044 Fee-debt entries (M5, superseded: path A and fee debt deleted, D-078)

- **Problem**: §8.4 specifies the trigger, recording and effect of fee debt, but not how the execution layer obtains the conflicting transaction or how a node discovers the conflict.
- **Decision**:
  - `BtcView` gains `transaction(txid)`: the inputs (outpoints) and outputs (amount, script) of a transaction confirmed on the anchor chain at a height not above the anchor height; otherwise `None`.
  - `Debt { op_id, conflict_txid }` is valid if and only if: the conflicting transaction is confirmed as above; some input i of it has an outpoint with a funding record in `0x07` whose recorded
    `op_id` equals the entry's `op_id`; the record is not yet marked as in debt; and the conflicting transaction has no i-th output, or its i-th output differs from the recorded paired output (amount and script).
    Execution: the signing account's `fee_debt += contribution_sat` (overflow makes it invalid), and the record is set to `debt = true`. The receipt's `op_id` is `None` and it counts 0 gas, the same as an unlock entry.
    Failing any of these makes the block invalid. A valid settlement transaction (including a rescue settlement) keeps the paired output at the same index, so it is never treated as a conflict.
    The implementation looks each input up in `0x07` in index order and takes the first input whose recorded `op_id` matches: the `op_id` determines a unique funding input and the inputs of a BTC transaction are distinct, so this is equivalent to "some".
    Which record to read depends on the conflicting transaction's inputs, so the view is queried before the state is read: on a view fault, any fee-debt entry is a local fault (an unlock entry, by contrast, first runs the checks that depend only on the state).
    `AnchoredView::transaction` is like `funding_output`: "absent" is trustworthy only once txindex has synced to the anchor height, and after the query it re-checks that the anchor block is still on the best chain.
  - How a node discovers conflicts: during the BTC scan it records the transactions (spender, height) that spend the funding inputs of `Merged` entries in "main-chain blocks that carry a settlement transaction but have no confirmed valid settlement yet";
    the producer writes fee-debt entries for conflicts that are confirmed at the anchor and not yet marked as in debt (the execution layer decides validity; invalid ones are skipped).
- **Spec gap (implemented per the original §8.4 text at the time; deleted together with path A and fee debt, D-078)**: "no debt is recorded when the conflicting transaction keeps the paired output" cannot distinguish misappropriation by a third party from front-running by the user themselves.
  A `SIGHASH_SINGLE|ANYONECANPAY` signature commits only to this one input/output pair; anyone who obtains it (including the user) can put it into their own transaction and add an output that takes
  the difference "funding amount − paired amount"; the user can also re-sign and place an identical paired output at the same index. In both cases the conflicting transaction keeps the paired output, so per the original text no debt is recorded,
  the user pays only the fee of that BTC transaction, the contribution returns to their own hands, and the producer bears the loss (the delayed broadcast of settlement transactions, D-037, leaves a time window for front-running).
  Comparing witnesses does not help either: the front-runner can reuse the user's original signature as is. Possible fixes: (1) drop the exemption and record debt for every conflict -- protects the producer, but anyone who sees the
  PSBT in gossip can saddle an honest user with debt; (2) restrict at the BTC layer how the funding output may be spent (i.e. the §14 M7 research item "account UTXO dual-signature lock"). Both go beyond the original §8.4 text
  and need a decision from the project owner; until then the execution layer follows §8.4, and the exemption test case in `tests/debt.rs` documents this behavior.
- **Impact**: `ksana-execution`, `ksana-btc` (tracker), `ksana-node` (block production); the indexer applies the same rules.

## D-045 Rescue settlement (M5, confirmed 2026-10-05, D-096; local policy, order per D-057)

- **Problem**: §7.3 allows anyone to rebuild a settlement transaction, but does not specify when a node does so or who initiates it.
- **Decision** (local policy, not consensus):
  - Once a main-chain block has reached `SETTLEMENT_DEPTH`, if the settlement transaction it carries cannot be broadcast (inputs spent or nonexistent) and there is no confirmed valid settlement yet, the node uses its own funding wallet to
    rebuild it: of the user input/output pairs it takes those whose funding output is still unspent (keeping the entry order), the payload is unchanged, it assembles and signs at this node's fee rate (the same `draft` / `finalize` as in block production),
    then broadcasts and tracks it like an ordinary settlement record (rebroadcast, reserved funding inputs, closed once confirmed).
  - The block's producer (`header.producer` is this node) rebuilds immediately; other nodes wait another 2 BTC blocks after finding it cannot be broadcast, and rebuild only if still no valid settlement has confirmed in the meantime,
    to reduce duplicate rescues. The same block may have several rescues, and the first to confirm takes effect (§7.3): rescues that still contain the same user inputs conflict with each other, and only one can land on BTC;
    when all user inputs are already invalid, each node's rescue spends only its own funding, they do not conflict, and they may all land on BTC; the extra ones merely pay fees for nothing (observed in the M5 regtest acceptance).
  - When the producer is offline, other nodes broadcast the settlement transaction carried by the block on its behalf as usual (D-037: any node broadcasts the settlement transactions of main-chain blocks that have reached the depth).
  - **Reserving funding inputs** (a race found in the M6 CI, fixed): transactions of this node that are in flight at the same time (the settlement transaction carried by a block, rescue settlements, the republish transactions of D-047)
    must not spend the same funding output, or one replaces the other in bitcoind's mempool (full-RBF is the default since Bitcoin Core 28). Previously, rescue and republish transactions
    reserved their funding inputs only after `sendrawtransaction` returned, while Bitcoin Core returns only after the transaction has entered the mempool and the validation queue's callbacks have been processed; during that interval
    the block production loop could pick the same output for a new block's settlement transaction (the republish test in `m5_regtest.rs` failed in CI because of this: the settlement transaction replaced the republish transaction, and the node gave up the republish and
    resent it). Now the block production loop (after choosing the settlement funding and before solving) and the rescue thread (before broadcasting) both "check and reserve" atomically with `Chain::try_reserve_inputs`;
    if already reserved, this attempt gives up and the next round chooses again; the inputs are released when the block production attempt ends (after the block is committed, the block's settlement record holds the reservation instead), released when the broadcast is rejected, and when the broadcast result is unknown (RPC error)
    the transaction is tracked as usual and verified in the next round. Regression test: `ksana-node/tests/m5_mock.rs::a_republish_in_flight_keeps_its_funding_input_out_of_new_settlements`
    (simulates bitcoind returning late; failed before the fix).
  - The producer's funding cache comes from a scan that only looks at the UTXO set: when a funding output is spent by a transaction this node no longer tracks (a republish or rescue transaction in flight before a restart, other spending from the funding wallet)
    and that transaction is still in the mempool, bitcoind rejects the settlement transaction. In that case the block production loop removes the funding outputs already spent in the mempool from the cache for this anchor, and the next attempt uses
    other funding (`a_funding_output_spent_in_the_mempool_is_skipped`; before the fix this node could not include any fast-path entries until that transaction confirmed).
- **Impact**: `ksana-node`.

## D-046 Settlement backlog limit (M5, confirmed 2026-10-05, D-096)

- **Problem**: §6.4 defines the backlog limit, but does not specify the BTC view used for the determination or the meaning of "valid settlement".
- **Decision**: for a new block N, let B be the lowest ancestor of N (excluding N) that carries a settlement transaction and has no valid settlement whatsoever
  (a confirmed settlement per D-034, including rescue settlements) on N's anchor chain at heights not above N's anchor height. If B exists and `N.anchor.height ≥ B.anchor.height + SETTLEMENT_TIMEOUT`, N may only be an empty block or
  a block containing only forced-inclusion entries; otherwise the block is invalid (a consensus rule that depends only on the anchor chain). When triggered, the producer produces only such blocks and logs a highest-level alert; it recovers automatically once B's settlement lands on BTC.
- **Impact**: `ksana-node` (validation and block production); the indexer applies the same rules.

## D-047 Header republishing (M5, confirmed 2026-10-05, D-096; local policy, including D-091 and D-095)

- **Problem**: §7.1 allows anyone to send a `Headers` payload to republish headers, but does not specify when a node sends one. A settlement payload carries at most `MAX_SETTLEMENT_HEADERS` headers;
  when more consecutive blocks than that carry no settlement transaction, or the chain has had no block carrying a settlement transaction for a long time, the settled prefix stalls and path C ops cannot become BTC-final either.
- **Decision** (local policy): after each round of BTC sync, the node finds the first main-chain block f whose data has not landed on chain (D-038). If f carries no settlement transaction:
  - there is already a block c after f that carries a settlement transaction: c's settlement payload carries the nearest `MAX_SETTLEMENT_HEADERS − 1` headers before it, so only when c is farther than that from f
    are the headers from f up to before `c − (MAX_SETTLEMENT_HEADERS − 1)` republished; otherwise wait for c's settlement to land on BTC (when stuck, rescue settlement, the backlog limit and CPFP deal with it; republishing would not help).
  - there is no block carrying a settlement transaction after f yet: when (a) at least `MAX_SETTLEMENT_HEADERS − 1` blocks already follow it (no later settlement payload will ever include f), or (b) f's anchor
    height is already more than `4 × SETTLEMENT_TIMEOUT` blocks behind the BTC tip (no block carrying a settlement transaction for a long time; this lets the settled prefix and the BTC finality of path C keep advancing),
    republish the headers from f on that have reached `SETTLEMENT_DEPTH`.
  The republish transaction is funded by the funding wallet: the headers are consecutive, without settlement transactions and not yet on chain, truncated to `MAX_BLOCK_DATA_BYTES`; the outputs are the change and the KSN1 data output.
  The same run of headers (distinguished by its starting height) is not sent again before the transaction confirms or is evicted.
  (Found in the M5 acceptance: the initial implementation looked only at the number of blocks after f, and when the settlement of an unlock block after f had not yet landed on BTC it also republished the headers that settlement would carry, paying a BTC fee for nothing.)
- **Impact**: `ksana-node`. Several nodes may each send one; duplicates are ignored by the indexer (§7.1).

## D-048 Standalone indexer (M5, confirmed 2026-10-05, D-096)

- **Problem**: §10.3 gives the indexer's procedure, but does not specify how block bodies are obtained, how settlement transaction validity is determined, or the output.
- **Decision**: `ksana-indexer` (library + binary) connects only to bitcoind and reads the same genesis config as the node:
  - It scans BTC with the same BTC tracker as the node (D-049): native transactions, headers in settlement and republish payloads, and each block's valid settlements. Headers are built into a tree by PoW and parent hash,
    and the main chain is chosen by cumulative work (on a tie, the one that appeared first is kept).
  - Block bodies: the entries of a block without a settlement transaction (D-043) are exactly the forced-inclusion entries derived per §7.4, verified against `entries_root`; the entries of the other blocks are taken from the KSN1 payload that claims to settle them,
    verified against `entries_root` (for forced-inclusion entries, the VmOp is fetched from BTC by `btc_txid`). Whether a block carries a settlement transaction is decided by "whether the `entries_root` of the forced-inclusion entries equals the header's
    `entries_root`". Settlement transaction validity is the same as in the node: the payload must equal the canonical payload determined by the chain (the headers from after the previous ancestor carrying a settlement transaction up to this block, at most
    `MAX_SETTLEMENT_HEADERS` of them, plus all entries of this block) and pass `validate_confirmed`; when a block has several valid settlements, the first to confirm prevails.
  - Only the main-chain prefix whose data has all landed on chain (D-038) is processed, executed block by block with `ksana-execution` (the same `BtcView` semantics), and the `state_root` is verified; on a mismatch it halts and alerts.
    When a BTC reorg takes already processed blocks out of the prefix, the state is rolled back. The state store is the same as the node's (`ksana-state`).
  - Output: logs, plus a library interface and JSON-RPC (`indexer_status`, `indexer_stateRoot`) for querying the block hash and `state_root` by height, for verification by tests and the devnet.
- **Implementation details** (decided during the M5 implementation; local behavior, no consensus change):
  - Header rules are the same as the node's `Chain::check_header` (`Consensus::check_header` plus anchor height not below the parent's); rules that depend on the reception time are not checked;
    headers with an unknown parent are held (no limit; BTC fees bound junk data) and attached level by level in order of appearance once the parent arrives; invalid ones are dropped. Accepted headers
    are not deleted on a BTC reorg (after a restart only those that appeared on the current BTC chain remain).
  - Deriving the forced-inclusion entries (block body, whether a settlement transaction is carried) requires the block's anchor to be on the best chain of the BTC snapshot, so that the blocks used are exactly those on the anchor chain; a block whose anchor block has been
    reorged out of BTC cannot be processed, and the indexer stops before it and waits (the indexer-side manifestation of the known limitation in D-035; resolved in M6.1 by the anchor-invalid mode of D-056).
    `Forced { btc_txid }` among the candidate entries corresponds by position to the derived forced-inclusion entries.
  - The settlement context (`KnownBlocks::settlement_context`) is answered with the canonical payload: during the tracker's scan only the last published snapshot can be used, and settlements that cannot be determined are
    recorded by the tracker as pending resolution and resolved with `resolve_all` after each sync round. When the canonical payload exceeds `MAX_BLOCK_DATA_BYTES`, no answer is given (that block cannot have a valid settlement).
  - Processable prefix: the longest prefix of the main chain from height 1 whose block bodies are known and whose data landing height is defined (i.e. `prefix_height` is defined), recomputed every round.
    Execution is the same as the node's `execute_checked`, plus the D-046 check (when triggered, the block may contain only forced-inclusion entries); the main-chain summary for `AnchoredView` is the executed blocks.
  - Halting: forced inclusion mismatch, backlog limit, execution judged invalid, `state_root` mismatch; the reason includes the height and the block hash. The halted state lives only in memory; after a restart
    execution starts over (the same block halts it again). Local faults (bitcoind, storage, data-source faults during execution) are retried in the next round.
  - Rollback: when executed blocks leave the main chain or the processable prefix, roll back to the highest height where both still agree; when the target is below the oldest version in the state store (pruned, keeping
    `REORG_WINDOW` versions, the same as the node), delete the state store and rebuild from genesis.
  - Restart: the BTC snapshot and the block tree live only in memory, so after a restart the scan starts from the beginning; the first round searches `[oldest version, min(latest version, processable height)]` for the highest version whose state root equals the main-chain block's
    `state_root` and rolls back to it; if none is found, it rebuilds from genesis. The data directory also has `indexer.json` (written as a temporary file and then renamed): the genesis config fingerprint
    (opening with a different config is refused), the genesis header (still obtainable after version 0 is pruned), and the most recently verified block (for the status shown after a restart and before verification).
  - Interface: `Indexer::{open, sync_once, status, block_at, find_block, request_stop}`; the JSON-RPC conventions follow `docs/RPC.md`:
    `indexer_status` → `{ height, hash, state_root, btc_height, halted }`, `indexer_stateRoot(block identifier: decimal height, "latest"
    or block hash)` → `{ height, hash, state_root }`; a block not yet executed and verified returns `-32004`. The `ksana-indexer` binary: `--data-dir`, `--genesis`,
    `--btc-rpc-url/-user/-pass`, `--rpc-addr` (default `127.0.0.1:8546`), `--poll-ms` (default 500).
  - Dependencies: only dependencies already in the workspace, no new external dependencies; the tests additionally use `ksana-node`, `ksana-mempool` and `tempfile` as dev-dependencies.
- **Impact**: new crate `ksana-indexer`; the devnet compose adds an indexer service.

## D-049 BTC tracking logic shared by the node and the indexer (M5)

- **Decision**: move the BTC snapshot, tracker, settled prefix and execution-layer view in `ksana-node/src/btc.rs` (`BtcSnapshot`, `BtcTracker`, `prefix_height`,
  `AnchoredView`, `KnownBlocks`) to `ksana-btc::tracker`; the error type becomes that module's own type, and the node and the indexer each implement `KnownBlocks`.
  Both giving exactly the same answers for the same BTC chain is the precondition for "the indexer and the fast-lane node agree on state".

## D-050 Fuzzing (SPEC §15)

- **Decision**: `fuzz/` is a separate cargo-fuzz workspace (like `contracts/`, it does not enter the main workspace's test / clippy) and needs nightly.
  New dependency `libfuzzer-sys` (the standard runtime of cargo-fuzz; §15 names cargo-fuzz explicitly), used only in `fuzz/`. Four targets:
  `vmop_decode` (VmOp decoding), `payload_decode` (payload decoding, KSN1 data scripts, payloads in transactions), `btc_inputs` (settlement transactions and path A PSBTs:
  decoding, settlement verification, rescue rebuilding, PSBT verification), `host_params` (a test contract calls the host functions one after another with the fuzzed input as parameters, checking that no `VmError` occurs,
  that two executions give the same result, and that gas does not exceed the limit). Besides not crashing, each target also checks properties such as canonical encoding and mutually consistent parse results (see `fuzz/README.md`).
- The seed corpus is generated by `fuzz/examples/seeds.rs` (validly signed VmOps, the three kinds of payload, signed settlement transactions and PSBTs, command sequences covering the success path of every host function),
  reusing the construction helpers from the `ksana-btc` and `ksana-vm` tests; the corpus and crash inputs are not committed to the repository.
- The CI `fuzz` job runs each target for 60 seconds starting from the seeds and uploads the triggering input when a crash is found. Longer runs are done locally as needed.

## D-051 CLI `ksana-cli` (SPEC §11.4; since D-106 the network is the node's)

- **Decision**: `crates/ksana-cli` is a client only: it queries and submits through the node's JSON-RPC, and paths A / C additionally connect to bitcoind. Commands: `key` (generate or read a private key,
  print the account and the P2TR / P2WPKH addresses; the private key file format is the same as `ksana-keytool`'s), `account` / `op` / `block` / `settlement` / `query` (read-only calls),
  `transfer` / `deploy` / `call` (one of three fee modes: `--fee-token` for path B; `--funding <txid>:<vout> --funding-key --contribution` for path A,
  which reads the funding output from bitcoind, paired output = funding amount − contribution, sent back to the original address, builds and signs the `SIGHASH_SINGLE|ANYONECANPAY` PSBT and then self-checks it;
  `--native` for path C, where the bitcoind wallet funds, signs and broadcasts the KSN1 output, confirming before broadcasting that the transaction can be recognized as a native transaction). `--wait` polls until the op is included.
  The nonce defaults to the account's next nonce on the node; the chain ID defaults to devnet. Results are always printed as JSON.
- No new external dependencies (`anyhow` is already in the workspace; `clap` enables the `env` feature so the RPC address can be taken from an environment variable).

## D-052 `Bip322Simple` signatures (M6, supersedes D-009, consensus rule)

- **Problem**: §4.2 only says "`Bip322Simple` is a BIP322 simple signature over the same message hash; the verification result is equivalent to `Bip340`: both prove that the caller controls the x-only public key".
  It does not specify the form of the message handed to the wallet, how the signature bytes are encoded, or which kind of address the x-only public key corresponds to.
- **Decision**:
  - **Message**: the **lowercase hex string** (64 ASCII bytes) of the VmOp sighash (the tagged hash of §4.2, `vmop_sighash`). The wallets' BIP322 interfaces
    (Unisat `signMessage(msg, "bip322-simple")`, Xverse `signMessage`) accept only text; BIP322's `message_hash` =
    `tagged_hash("BIP0322-signed-message", these 64 bytes)`.
  - **Signature bytes**: `Bip322Simple(Vec<u8>)` is the BIP322 simple signature itself, i.e. the Bitcoin consensus encoding of the `to_sign` input's witness stack (the bytes obtained by base64-decoding what the wallet returns,
    without the `smp` prefix of the newer BIP322 revision), at most 1,024 bytes long and fully consumed.
  - **Address** (`message_challenge`) is determined by the account and the witness; only two forms are accepted:
    - Witness with exactly 1 item: P2TR key-path; the address is the P2TR with the account as internal key, tweaked per BIP86 (no script tree). The item is a 64-byte signature (`SIGHASH_DEFAULT`) or
      65 bytes with `0x01` as the last byte (`SIGHASH_ALL`), verified against the BIP341 sighash (`to_sign` input 0, prevout = the output of `to_spend`: amount 0, that P2TR script)
      with the tweaked output key (Unisat taproot accounts, Xverse ordinals addresses).
    - Witness with exactly 2 items: P2WPKH; the 2nd item is a 33-byte compressed public key whose x coordinate equals the account, and the address is that key's P2WPKH; the 1st item is a strict-DER, low-S ECDSA signature plus
      `0x01` (`SIGHASH_ALL`), verified against the BIP143 sighash (scriptCode = that key's P2PKH script, amount 0) (Unisat native segwit accounts).
    - Any other form (P2SH-P2WPKH, P2PKH, multisig, other sighash types) always fails verification (most conservative).
  - `to_spend` / `to_sign` follow BIP322 strictly: version 0, `nLockTime` 0, `nSequence` 0; the `to_spend` input spends `000…000:0xFFFFFFFF`,
    scriptSig = `OP_0 PUSH32[message_hash]`, and `to_sign` has a single `OP_RETURN` output.
  - The official BIP322 test vectors (the p2wpkh and p2tr simple cases and the error cases from `bip-0322/basic-test-vectors.json` and `generated-test-vectors.json`)
    are stored in `crates/ksana-crypto/tests/data/bip322/` and checked one by one; the shared vectors `tests/vectors/signing.json` gain `bip322` cases (P2TR and P2WPKH,
    one each; signature determinism: BIP340 uses a fixed `aux_rand`, ECDSA follows RFC 6979) for the TS SDK to check the message and the witness.
- **Impact**: consensus change -- previously `Bip322Simple` always failed verification (D-009), so no chain can contain such an op; existing chains and the determinism golden roots are unaffected.
  `ksana-crypto` gains the dependency `bitcoin` (in the §12 recommended list): used for witness decoding and the BIP341 / BIP143 sighashes; the tests additionally use `base64` (dev-dependency only,
  to decode the signatures in the official vectors).
- **Implementation note**: the public key in a P2WPKH witness must be exactly 33 bytes -- `CompressedPublicKey::from_slice` also accepts the 65-byte uncompressed encoding and treats it as the same key,
  while P2WPKH takes hash160 of the raw bytes in the witness, so the uncompressed encoding corresponds to a different address (found in testing). All P2WPKH and P2TR simple signatures of the official BIP322 vectors pass,
  and the error cases and unsupported types such as P2WSH all fail.

## D-053 TypeScript SDK (M6, §11.3; no default network since D-106)

- **Decision**: `sdk/ts`, package name `@ksana/sdk` (`private`, not published), ESM + TypeScript; the library code uses only Web standard APIs (`fetch`, `crypto.getRandomValues`),
  shared by Node.js (≥ 20) and browsers. Dependencies: `@noble/curves`, `@scure/btc-signer`, `borsh` as recommended by §11.3, plus their sibling packages `@noble/hashes`, `@scure/base`;
  tests use Node's built-in `node:test`. Versions are locked in `sdk/ts/package-lock.json`.
- Modules: consensus types and Borsh encoding / decoding (length limits checked first, full consumption required), the JSON representation of D-007, hashes (tagged hash, sighash, `op_id`, block hash,
  `entries_root`), BIP340 signing and verification, BIP322 messages (D-052), path A PSBTs (construction, `SIGHASH_SINGLE|ANYONECANPAY` signing, extracting the `Contribution`),
  path C KSN1 data scripts and transactions, JSON-RPC clients for the node and the indexer, wallet adapters (Unisat, Xverse) and a mock wallet for tests.
- The shared vectors in `tests/vectors/` are checked one by one by the SDK tests (encoding, decoding failure cases, signatures, tampering, Merkle, BIP322); CI gains a `ts sdk` job.
- The Xverse adapter calls its injected `XverseProviders.BitcoinProvider.request` directly (the same JSON-RPC methods as sats-connect) without pulling in sats-connect.
- **Implementation notes** (M6, details not covered by D-052–D-055):
  - Versions (pinned exactly): `@noble/curves` 2.4.0, `@noble/hashes` 2.4.0, `@scure/btc-signer` 2.4.1, `@scure/base` 2.4.0, `borsh` 2.0.0
    (`@scure/btc-signer` also brings `micro-packed` 0.11.1); dev dependencies `typescript` 5.9.3, `@types/node` 22.20.4. TypeScript 7.x is not used: 7.0 is the native Go compiler,
    its `tsc` is provided by per-platform optional dependencies, and a cross-platform `npm ci` from `package-lock.json` is not stable enough; 5.9 is a pure JS implementation. Node ≥ 20.19 (required by `@noble/curves` 2.4), CI uses Node 22.
  - TS types: field names are the same as serde's (snake_case), enums are union types with a `kind` field (the contents of tuple variants go in the `bytes` field), `u64` / `u128` are `bigint`.
  - `borsh` 2.0.0 does not check integer ranges, does not check that the input is fully consumed, and its UTF-8 decoding is not strict (an out-of-range enum index only happens to throw a TypeError), so the SDK wraps it with
    per-field validation and, after decoding, re-encodes and requires the result to be byte-for-byte identical to the input (ruling out trailing bytes and every non-canonical encoding), consistent with the canonicality on the Rust side.
  - External PSBTs (`contributionFromPsbt`) are parsed with the SDK's own strict BIP174 v0 parser, with the same rules as the node's rust-bitcoin 0.32 (v0 only, no duplicate keys, the number of maps matches the transaction,
    full consumption): the `@scure/btc-signer` parser accepts one extra trailing `0x00` and PSBT v2. Building and signing PSBTs still uses `@scure/btc-signer`;
    for the wallet's own P2TR funding input the PSBT carries `PSBT_IN_TAP_INTERNAL_KEY` (the Rust `build_psbt` does not write it, and the node ignores the field) to make signing easier for the wallet.
  - Self-checks (not consensus rules; the node is authoritative): before submitting, `buildPathAOp` verifies the wallet-signed PSBT per D-033 (`verifyContribution`) and the wallet's BIP322 signature per D-052;
    `verifyVmop` returns `unsupported` for `Bip322Simple`.
  - The call form of `ksana_estimateFee` matches the node: the only parameter is an optional request object `{ max_gas?, op_bytes?, funding? }` (only the given fields are written out,
    and no parameter at all when everything is defaulted); the result includes `max_gas`. The path A flow estimates `op_bytes` from the BIP322 witness length of the address type.
  - Path C `buildNativeTx`: output 0 is the KSN1 data output, output 1 the change (omitted and added to the fee when below the dust limit); P2TR / P2WPKH UTXOs are used in the given order;
    weight is estimated with 65-byte P2TR signatures and 72-byte DER signatures; `nSequence` defaults to `0xFFFFFFFD` (RBF-enabled); rejected above 400,000 WU.
  - Xverse uses the ordinals (P2TR) address by default: the payment address is usually P2SH-P2WPKH, which D-052 does not support; Xverse's `signPsbt` does not accept per-input sighash types,
    it signs according to the PSBT input's `sighash_type` (`0x83` for path A).
  - A second copy of the official BIP322 vectors lives in `sdk/ts/test/data/bip322/` (byte-for-byte identical to `crates/ksana-crypto/tests/data/bip322/`). Of the two official P2WPKH signatures,
    the 72-byte one is exactly the RFC 6979 deterministic signature (the other is a low-R signature) and the SDK reproduces it byte for byte; the official P2TR signature used a random `aux_rand` and is only verified.

## D-054 `ksana_estimateFee` (M6, §8.2, §11.2)

- **Problem**: §8.2 gives the formula for the suggested path A contribution (virtual bytes occupied × current fee rate + `max_gas × min_gas_price_msat / 1000`) and "path B is based on recent blocks",
  without specifying the parameters, how the virtual bytes are computed, or the value of `min_gas_price_msat`.
- **Decision**:
  - Parameters (all optional): `max_gas` (decimal string, default 1,000,000), `op_bytes` (the byte count of the op's Borsh encoding, default the size of one `AssetTransfer` op),
    `funding` (`"p2tr"` or `"p2wpkh"`, default `"p2tr"`; the paired output takes the same type).
  - Virtual bytes occupied = ⌈(weight of the funding input + weight of the paired output + 4 × the entry's byte count in the settlement payload) / 4⌉: P2TR input 164 + 67 WU, P2WPKH input
    164 + 108 WU (estimated with a 72-byte DER signature); P2TR output 172 WU, P2WPKH output 124 WU; the byte count of a `Merged` entry in the payload = 1 + `op_bytes` +
    the encoded length of `Contribution` (computed with the witness above). The current fee rate is the fee rate this node uses to fund settlement (not below `MIN_SETTLEMENT_FEERATE`).
  - `min_gas_price_msat` is node local policy (`Policy::min_gas_price_msat`, default 1 msat / gas, `--min-gas-price-msat`).
  - Path B: the median `amount` of the path B entries in the last 20 main-chain blocks, not below this node's `min_token_fee`; `min_token_fee` when there are no path B entries.
  - Result: `{ "feerate_sat_vb", "min_gas_price_msat", "path_a": { "vbytes", "contribution_sat" }, "path_b": { "amount" }, "at" }`.
  - Suggestions only: the node's conditions for accepting an op are unchanged (`min_contribution_sat`, `min_token_fee`).

## D-055 signet and the browser wallet demo (M6 acceptance item 2)

- **Problem**: M6 acceptance item 2, "complete one path A op on signet with Unisat or Xverse", needs a real browser wallet extension, signet coins and a publicly reachable signet node,
  and cannot be completed automatically in CI or in the dev container.
- **Decision**:
  - The protocol parameters gain `SIGNET` (differs from `DEVNET` only in `chain_id` = 1002); the node, the indexer and the CLI gain `--chain devnet|signet`.
  - The node RPC gains `--rpc-cors <origin>` (CORS, for cross-origin calls from browser pages; `*` or a comma-separated list of origins, not allowed by default), with the new dependency `tower-http`
    (cors feature, the HTTP middleware approach from the jsonrpsee docs) and explicit dependencies on the `tower` and `http` it needs (both are already dependencies of jsonrpsee).
  - `sdk/ts/examples/wallet-demo`: a static page that connects to Unisat or Xverse, reads the account, requests a fee estimate, picks the funding input, has the wallet sign the PSBT and the BIP322 message,
    submits and tracks the status. `docs/SIGNET.md` spells out the steps to deploy a signet bitcoind, the node, the indexer and the page and to complete the acceptance.
  - Automated substitute verification: the SDK's mock wallet implements the same interface as Unisat (`signPsbt`, `signMessage(…, "bip322-simple")`) and walks the whole of
    path A (PSBT + BIP322-signed VmOp) on regtest until the settlement lands on BTC. The one op with a real wallet on signet is ticked off by the project owner after performing it per `docs/SIGNET.md`.
- **Implementation notes** (M6):
  - The demo page only calls the SDK (`submitPathA` etc.) and is bundled by esbuild into a single ESM file (`npm run build:demo`). When the page URL carries
    `mock=p2tr|p2wpkh&key=<private key>`, the SDK's mock wallet (`MockUnisatWallet`) is injected; only for local regtest.
  - The SDK gains two dev dependencies (runtime dependencies unchanged): `esbuild` 0.28.2 (bundles the demo page); `playwright-core` 1.56.1 (`test/e2e/demo-page.ts` drives the page in Chromium;
    it ships no browser itself: CI installs the headless Chromium matching that version with `npx playwright-core install --with-deps --only-shell chromium`;
    1.56.1 is pinned because the Chromium preinstalled in the dev container corresponds exactly to this version).
  - The automated substitute verification is in `tests/integration/tests/m6_regtest.rs`: (1) in Node, the mock wallet goes through path A via the Unisat adapter (P2TR and P2WPKH, checking that the settlement transaction
    carries the wallet-signed funding input and paired output unchanged, until btc_final) and path C (the wallet-signed native transaction is force-included); (2) the demo page in Chromium goes through
    path A calling the node via CORS. The page check found a problem with the SDK calling `fetch` in the browser with the client as `this` (browsers throw Illegal invocation, Node does not check),
    which has been fixed, with a regression test added.

## D-056 Anchor depth and anchor-invalid historical blocks (M6.1, revises D-035 and D-048; consensus change)

- **Problem**: the known limitation of D-035. Producers anchor to the BTC tip, and during the roughly 10 minutes a BTC block is the tip, about 120 fast-lane blocks anchor to it. After a 1-block reorg turns it into an orphan:
  nodes that join later or resync judge the blocks anchored to it invalid (`btc_anchor is not on the local BTC best chain`) and cannot keep up with the main chain; the indexer stops before these blocks (D-048).
  Fast-lane blocks already accepted by the network can thus no longer be verified independently, violating "fast lane first, BTC in sync": once accepted, a fast-lane block is valid forever and can be verified afterwards.
- **Decision**:
  - **Anchor depth**: new protocol parameter `ANCHOR_DEPTH` (`Params::anchor_depth`). The producer anchors to the block at `max(parent's anchor height, local BTC tip − ANCHOR_DEPTH)`.
    When receiving a new block: the anchor block is on the local BTC best chain (unchanged); the anchor height does not exceed `max(parent's anchor height, local tip − ANCHOR_DEPTH)`, otherwise it is treated as
    the local BTC being behind and retried later (same as "anchor above the local tip"); local tip − anchor height ≤ `MAX_ANCHOR_LAG + ANCHOR_DEPTH`.
    devnet uses 0 (same behavior as before), signet 1, mainnet suggested 2. Cost: a path A funding input needs `ANCHOR_DEPTH + 1` confirmations before it can be included,
    and the effective confirmation count for forced inclusion is `ANCHOR_DEPTH + NATIVE_CONFIRMATIONS` (mainnet can lower `NATIVE_CONFIRMATIONS` accordingly); path A and B ops still take effect at block production.
  - **Anchor-invalid mode**: "the anchor block is on the local BTC best chain" is checked only when receiving a new block (like `MAX_ANCHOR_LAG` and the timestamp rules, §6.3). When syncing historical blocks
    (P2P sync, attaching side-chain blocks during a reorg, crash recovery) and in the indexer, blocks whose anchor block is not on the current best chain but is not higher than the local tip are processed as usual:
    - the rules that can only be reproduced on the anchor chain are no longer checked: whether the forced-inclusion entries are complete (§7.4), the settlement backlog limit (§6.4);
    - the BTC view used by the execution layer and by settlement transaction verification answers from the current best chain instead: block headers are taken by height from the current chain; funding outputs, conflicting transactions and settlement confirmations are looked up by transaction ID on the current chain,
      at any height (the content of a given transaction ID does not change, so the execution result is the same as on the anchor chain); not found is a local fault (retry later), never grounds to judge the block invalid;
    - signatures, nonces, PoW, the entries root, the structure and payload of the settlement transaction, the execution result and `state_root` are checked as usual.
    Security: when these blocks were accepted, the whole network had already checked them against the anchor chain; the syncing party relies on the fast lane's own hashrate, consistent with §16 "the security of fast-lane confirmation comes from the fast lane's own hashrate".
  - **The indexer's block body** (when anchor-invalid): the body is empty when `entries_root` is the root of empty entries; otherwise take, from the payloads claiming to settle the block, the candidate whose converted `entries_root` matches
    (`EntryData::Forced` looks up the native transaction by transaction ID on the current chain); when there is none, derive the forced-inclusion entries from the current chain over the same height range and adopt them if they match;
    wait (recording the reason) when it still cannot be determined.
  - **Not implemented for now, recorded as remaining limitations**: (1) forced-inclusion entries in the settlement payload carrying the VmOp: with anchor depth and lookups by transaction ID, it is only useful when a reorg deeper than
    `ANCHOR_DEPTH + NATIVE_CONFIRMATIONS` is followed by a double-spend of the native transaction, which is out of proportion to the cost of a payload format change (shared vectors, SDK); it is left to be considered together with
    the move of the data carrier to witness envelopes; (2) catch-up inclusion of native transactions moved before the already-processed range by a reorg: needs new state and inclusion rules, and likewise only arises in deep reorgs.
    For a block that contains only forced-inclusion entries, is anchor-invalid and whose native transactions changed height on the new chain, the indexer still waits.
    (M29, D-104: a block whose native transaction a deep reorg moved or double-spent now leaves the main chain and the window restarts below the new height, which solves (2) and leaves (1) without a use.)
- **Impact**: `ksana-primitives` (parameters), `ksana-btc` (the anchor-invalid mode of `AnchoredView`), `ksana-node` (anchor rules, block production, verification and execution of historical blocks),
  `ksana-indexer` (block bodies, execution).
- **Implementation**:
  - Parameter: `Params::anchor_depth`, devnet 0, signet 1 (`SIGNET` and `DEVNET` differ only in `chain_id` and this); the `MAX_ANCHOR_LAG` check becomes
    `MAX_ANCHOR_LAG + ANCHOR_DEPTH`. The producer anchors to the block at `max(parent's anchor height, snapshot tip − ANCHOR_DEPTH)` (taking the hash at that height from the snapshot).
  - "Receiving a new block" vs. "syncing a historical block" follows the node's existing distinction (D-041): `accept_block` with `now` given (blocks received via gossip) is a new block; `None`
    (P2P sync responses, orphans attached along with their parent, side-chain blocks attached during a reorg, crash recovery, import) is a historical block. Only new blocks check that the anchor block is on the best chain, the anchor depth and
    the lag; a historical block's anchor is only required to be no higher than the locally synced tip.
  - `AnchoredView::history` (for historical blocks and the indexer): anchor-invalid mode when the anchor block is not on the snapshot's best chain and its height does not exceed the snapshot tip; `AnchoredView::new`
    (block production, new blocks, RPC) is unchanged: an anchor not on the snapshot's best chain is a local fault. In anchor-invalid mode: transactions and funding outputs are looked up by transaction ID on bitcoind's current best chain
    (any height; not on the best chain is a local fault); settlement confirmations and the settled prefix are judged on the current chain (any height), a block not on this chain is answered "no" as usual,
    and the other cases that do not hold are local faults; for block headers, when bitcoind knows the invalidated anchor block, walk back along parent hashes, taking the blocks on the invalidated branch passed during the walk directly and lower ones from the current chain,
    and answer from the current chain when bitcoind does not know the anchor block. Contracts can only read block headers below anchor height − `BTC_QUERY_DEPTH`, and only a BTC reorg deeper than that would
    give a wrong answer; therefore, when re-executing an anchor-invalid block yields a mismatching `state_root`, both the node and the indexer treat it only as a local fault (not judged invalid, retried later).
  - **Threshold for a historical block to enter anchor-invalid mode** (a hole found during implementation; tightens the decision above): "historical blocks" include P2P sync responses and orphans attached only later, and a freshly produced
    block can also be sent out only through a sync response -- with a forged anchor hash (never a BTC block) a producer could bypass the new-block anchor, forced inclusion and backlog checks. The node therefore handles a historical block whose anchor is not on the best chain in anchor-invalid mode only in
    the following cases: bitcoind knows the anchor block (height matches, not on the best chain, i.e. a real block that was reorged out),
    or the anchor is more than `MAX_ANCHOR_LAG + ANCHOR_DEPTH` behind the local BTC tip; otherwise it is rejected (`Rejected`, not judged invalid, retried later). A freshly produced block's anchor
    is no lower than its parent's and at most `MAX_ANCHOR_LAG + ANCHOR_DEPTH` behind the tip, and a forged hash can satisfy neither; by the time its anchor is deep enough, honest producers have long since
    produced many blocks on the original chain, and such a fork that nobody accepted when it was produced does not have enough work. A freshly synced bitcoind does not know reorged-out blocks, but by then the anchor is most likely already
    deep enough (otherwise wait for BTC to advance a few more blocks). The indexer only processes the main chain selected by work whose data is already on BTC, with no extra threshold. The import test in `m3_mock.rs` now
    asserts rejection only for new blocks; `m6_1_anchor.rs` covers the two threshold conditions and the rejection of forged hashes separately.
  - Indexer: `Known::body` / `carries` determine the body of an anchor-invalid block in the order decided above (the "cannot be determined yet" reason is logged only once per block),
    `carries` is decided by whether the body contains entries other than forced ones; `BtcSnapshot::native_tx` finds the native transaction on the current chain by transaction ID.
  - Tests: `crates/ksana-btc/tests/anchored_view.rs` (the individual answers in anchor-invalid mode), `crates/ksana-node/tests/m6_1_anchor.rs` (the production and
    reception rules of anchor depth; a new node importing from genesis after the anchor block was reorged out), `crates/ksana-indexer/tests/indexer.rs` (a force-included native transaction and a path A
    funding transaction change height on the new branch; both the indexer and a new node get past the anchor-invalid blocks), `tests/integration/tests/m6_1_regtest.rs` (regtest acceptance).

## D-057 Rescue order and deduplication of rescue settlement (M6.1, supplements D-045; local policy)

- **Problem**: in D-045, when all user inputs have become invalid, each node's rescue spends only its own funding and they do not conflict with each other, so all of them may land on BTC and the extra ones are just wasted fees.
- **Decision** (local policy, not part of consensus):
  - The order follows the fast-lane main chain: the block's producer rescues immediately; if the k-th block after it (k = 1…`RESCUE_ORDER_WINDOW` = 6) was produced by this node, it waits
    k BTC blocks after finding that broadcasting is impossible; other nodes wait `RESCUE_ORDER_WINDOW + RESCUE_DELAY`. Every node can compute its own place from the chain, and those earlier in the order rescue first.
  - Check the BTC mempool before initiating: do not initiate when there is already a KSN1 transaction claiming to settle the block (the last block header of the payload is this block). Only query when there are blocks awaiting rescue
    (`getrawmempool`, fetching unseen transactions one by one, results cached).
- **Impact**: `ksana-btc` (`BtcClient::mempool_txids`), `ksana-node` (`rescue.rs`).

## D-058 Local list of path A fee-evading accounts (M6.1, supplements D-044; local policy)

- **Problem**: a spec hole in D-044 -- "no fee debt is recorded when the conflicting transaction keeps the paired output" cannot distinguish misappropriation by a third party from front-running by the user themselves, so a user can use it to take the contribution back for free.
  Removing the exemption (always recording fee debt) would let anyone who sees the signed input on P2P take the contribution and then saddle the honest user with a fee debt, so the consensus rule keeps the original text of §8.4.
- **Decision** (local policy, not part of consensus): when the node finds during the BTC scan that a path A funding input was spent by a transaction that "keeps the paired output at the same index" (i.e. the exemption case of §8.4)
  and the block has no confirmed valid settlement yet, it puts the op's account on a local list (persisted in the node's storage); from then on the entry pool refuses path A ops from that account (paths B and C are unaffected).
  `ksana_getAccount` gains a `path_a_refused` field (this node's policy, not consensus state). The list only affects this node's entry pool admission, not block validity.
- **Cost**: an honest user whose input was misappropriated by a third party also loses path A on this node (paths B and C remain usable); this is the price of making "front-running fee evasion" unprofitable with local policy. The real cure remains
  §17 "double-spend protection for path A" (the dual-signature lock of account UTXOs in M7).
- **Impact**: `ksana-node` (BTC scan, storage, entry pool admission, RPC), `ksana-rpc`, `sdk/ts` (RPC types).
- **Implementation**: after each sync round, the node's BTC thread checks the spend records of the watched funding inputs (D-044) in the snapshot: KSN1 transactions are skipped; the funding record in the state at the main-chain tip decides
  whether the spending transaction keeps the paired output at the same index (retried next round when the transaction cannot be found); nothing is recorded when the op's block already had a confirmed valid settlement before this spend
  (the spend did not compete with the settlement). The list is persisted in the `policy` column family of the block store (account → triggering op, funding input, spending transaction and its confirmation height)
  and is not revoked on BTC reorgs. Refusal happens at entry pool admission (RPC returns `-32011`; P2P relays are reported as Ignore, with no penalty for the relaying peer); before packing, the producer removes the listed accounts'
  path A ops already in the pool from the entry pool. `ksana_getAccount.path_a_refused`, the SDK's `AccountInfo.path_a_refused` (`false` when an old node
  does not return it) and the wallet demo page show this status. Test: in `crates/ksana-node/tests/m5_mock.rs`,
  `front_running_a_settlement_with_the_paired_output_kept_refuses_later_path_a_ops`.

## D-059 Peer rate limiting (M6.2, SPEC §16; local policy)

- **Problem**: the mitigation §16 gives for "mempool junk ops" is "verify the signature, balance and contribution at submission; rate-limit per account and per peer". Per account there is already
  `max_ops_per_account` (the number of queued ops per account and the nonce look-ahead distance; a replacement must raise the fee), but the network layer only deducts score for invalid messages: a peer can send, at high speed,
  well-formed ops that this node will Ignore (nonce already used, insufficient balance, etc.), or send `GetBlocks` / `GetMempool` at high frequency, making this node repeatedly decode, verify signatures,
  read storage and assemble large responses.
- **Decision** (`ksana-p2p`, `Tuning::peer_limits`): three token buckets per connected peer -- gossip blocks, gossip ops, inbound sync requests --
  configured as "refill per second + capacity" and computed in integers (thousandths of a token). Defaults: blocks 20 / s, capacity 100; ops 1000 / s, capacity 5000; sync requests
  50 / s, capacity 200, where `Status` and `GetBlock` count 1 each and `GetBlocks` and `GetMempool` count 10 each (their responses can reach 32 MiB).
  Gossip messages over quota are reported as Ignore before decoding (not forwarded, no score deducted); requests over quota are dropped outright (the peer sees a failure); sync
  responses received to our own requests are not counted. A message that needs more tokens than the capacity is charged the capacity (it needs a full bucket), so however small the configured capacity, no kind of request is refused forever. Tokens are refilled only in whole milliseconds,
  with the fraction below one millisecond carried over to the next time, so frequently arriving messages cannot keep the bucket from ever refilling. Honest nodes only exceed the limits occasionally because of bursty forwarding, hence no score is deducted; the number of dropped items is logged at most once every 10 seconds per peer. The token buckets are deleted on disconnect.
  Since D-132 (2026-10-28) the buckets belong to the source, the address a peer connects from, are shared by every identity connecting from it and outlive the connections.
- **Cost**: gossipsub deduplicates by message ID, so a message dropped by rate limiting is no longer processed either when it is received again later from another node; only block inclusion or a later entry pool sync can make it up;
  the default quotas are far above the forwarding rate of honest nodes (devnet: one block every 5 seconds).
- **Impact**: `ksana-p2p` (`Tuning`, event loop); the node uses the defaults.

## D-060 Separating the settlement funding wallet from the revenue wallet (M6.2, SPEC §16; local policy)

- **Problem**: §16 requires that "the producer's settlement funding wallet be separate from its revenue wallet, limiting the amount of funds in the hot wallet". The producer's BTC income (what remains of path A contributions after
  deducting fees) all goes into the settlement transaction's change, and the change has always been paid back to the funding address: the hot wallet's balance only ever grows.
- **Decision**: the settlement transaction's change is only required to be a standard output script (D-034); where it is paid is the producer's choice. The node adds an optional revenue address and hot-wallet cap
  (`--revenue-address`, `--hot-wallet-cap-sat`, default 1 000 000 sat): when assembling this block's settlement transaction, if the total of the hot wallet's UTXOs usable for funding, minus
  the funding inputs chosen this time, is still not below the cap, the change is paid to the revenue address; otherwise it returns to the hot wallet. The hot wallet's balance thus stays near the cap (plus one change output), and the excess
  moves to the revenue address (which can be a cold wallet) with the settlement transaction, without any extra transaction. "Usable" is the same as for funding selection: confirmed before the anchor height, not occupied by an unconfirmed settlement or rescue,
  and not an immature coinbase. The revenue address must belong to this chain's BTC network and must not be the funding address itself.
- **Scope**: only affects the settlement transactions the producer assembles for its own blocks; rescue settlement keeps the original transaction's change output (D-045), and the change of header republishing transactions still returns to the hot wallet.
- **Impact**: `ksana-node` (`Funding::revenue`, the block production loop, the command line).

## D-061 Duration of the soak test (decided by the project owner, revises SPEC §15)

- **Problem**: SPEC §15 requires a 3-node devnet to run continuously for 24 hours. A single job on a GitHub-hosted runner is limited to 6 hours; 24 hours can only be run on one's own machine.
- **Decision** (the project owner: "6 hours it is, then"): the soak test runs for as long as a GitHub-hosted runner can. The job limit of `soak.yml` (manually triggered) is 350 minutes,
  of which building the images takes about 30 minutes and the soak itself at most 18 000 seconds (5 hours); the default duration of `devnet/soak.sh` changes to 18 000 seconds accordingly. CI's devnet job still runs for 3 minutes
  after the smoke test. When a longer run is needed, set `KSANA_SOAK_SECONDS` on your own machine.
- **Impact**: `devnet/soak.sh`, `.github/workflows/soak.yml`, CLAUDE.md.

## D-062 Decoupling the mining algorithm from consensus (M7, decided by the project owner, revises SPEC §6)

- **Problem**: the project owner's decision (2026-10-01) on §17 "the long-term form of fast-lane consensus": fast-lane consensus switches to a DAG; the mining algorithm is decoupled from consensus, and which algorithm to use is not yet decided.
  The existing code hard-codes SHA256d in the PoW check, solving and chain work.
- **Decision**:
  - `ksana-consensus` adds the `PowAlgorithm` trait: `pow_value(header) -> U256` (the value compared against the target; the smaller, the harder to obtain) and the nonce-searching
    `solve(header, target, start, iterations)`. The protocol parameter `Params::pow_algorithm` (an enum, currently only `Sha256d`) selects the algorithm per network
    and is a consensus parameter; changing the algorithm is a consensus change (a new network or a planned upgrade; no upgrade mechanism in this phase).
  - Algorithm-independent parts that stay unchanged: the block hash (the block identifier) is still `sha256d(borsh(header))`; the compact encoding of `bits`, the target, the work
    `⌊2^256 / (target + 1)⌋` (D-025) and ASERT (D-026) are all defined in terms of the "target" and do not depend on the algorithm; GHOSTDAG (D-063) uses only the work.
    `Sha256d`'s `pow_value` is just the block hash interpreted little-endian (same as D-025; existing chains are unchanged), and solving keeps the midstate optimization.
  - The block header still carries the solution in `nonce: u64`: enough for most hash-based algorithms; algorithms that need a larger solution (Equihash, Cuckoo) or auxiliary proof of work (AuxPoW for merged mining with BTC pools:
    the BTC block header, coinbase and Merkle branch) require a new block header version, to be decided together with the algorithm at that time.
- **Impact**: `ksana-consensus` (the trait, `Sha256d`, `PowConsensus`, solving), `ksana-primitives` (parameters); the node, indexer and CLI go through
  `PowConsensus` and do not call SHA256d directly.
- **Implementation (M7.8)**: the node (`NodeConfig::pow_algorithm`, `Chain::open_with_algorithm`) and the indexer (`IndexerConfig::pow_algorithm`)
  can use a given algorithm in place of the one selected by the network parameters (default `None`). This is only an injection point for tests: the command line does not expose it, and real networks are still governed by `Params::pow_algorithm`.
  The tests use an algorithm that implements only `pow_value` (a single SHA-256 interpreted big-endian) to produce blocks, verify them, sync them over the network and rebuild them in the indexer, consistent with the node; nodes and indexers
  using SHA256d reject these blocks (`ksana-node/tests/m7_algorithm.rs`, `ksana-indexer/tests/indexer.rs`).

## D-063 Fast-lane consensus switches to a DAG: GHOSTDAG (M7, decided by the project owner, revises SPEC §6, §7.1, §7.4, §8.5, §10.3, §11.1)

- **Problem**: the §17 decision (the same one as D-062) requires the fast lane to switch to DAG consensus to eliminate orphans (blocks produced in parallel are all merged, no hashrate is wasted, and there are no longer mutually conflicting
  branch settlements). The rest of the spec (execution, settlement, forced inclusion, settled prefix, indexer, sync) is written for a linear chain and needs to be rewritten item by item.
- **Protocol**: GHOSTDAG (Kaspa's definition and algorithm). Parameters (devnet, `Params`): `GHOSTDAG_K = 8`, `MAX_BLOCK_PARENTS = 10`,
  `MAX_MERGESET_SIZE = 128`, `MERGE_DEPTH = 720` (in blue score); the block interval is still `TARGET_BLOCK_TIME` (5 seconds) and can be lowered later.
  - Each block's work `work(B)` follows D-025; `selected_parent(B)` = the parent with the largest `(blue_work, hash)` (hashes compared byte by byte over 32 bytes);
    `mergeset(B)` = `past(B) \ past(SP)` (including SP itself), ordered, apart from SP, by ascending `(blue_work, hash)` (which is a topological order); coloring follows Kaspa's
    k-cluster greedy algorithm (SP is blue, `mergeset_blues` has at most K + 1 blocks); `blue_score(B) = blue_score(SP) + |mergeset_blues|`,
    `blue_work(B) = blue_work(SP) + Σ work(mergeset_blues)`, both 0 for the genesis block; `daa_score(B) = |past(B)|`.
    `blue_work` strictly increases along every parent-child edge, so an ancestor query walks the queried block's selected chain checking each merge set (stopping once `blue_work` falls below the sought block's), and no separate reachability index is needed.
- **Block header v2** (`BLOCK_VERSION = 2`): `parent: [u8; 32]` becomes `parents: Vec<[u8; 32]>`; the other fields and their order are unchanged (`nonce` is still last):
  - A non-genesis block has 1 to `MAX_BLOCK_PARENTS` parents: `parents[0]` must be the selected parent computed by the rule above, and the rest are in strictly ascending byte order and not equal to `parents[0]`;
    no parent may be in another parent's past; the genesis block's `parents` is empty.
  - `height` becomes the "selected-chain depth": `parents[0].height + 1` (0 for the genesis block). Parallel blocks can have the same height; the main-chain height index, state versions and the `block_height`
    host function all follow it; on a pure chain it equals the former height.
  - `bits` = ASERT (D-026) computed with `daa_score(B) − 1` as the parent height and `parents[0]`'s timestamp as the parent time (same as before on a pure chain).
  - Merge limits (invalid block): `|mergeset(B)| ≤ MAX_MERGESET_SIZE`; for every block in `mergeset(B)` other than SP, `blue_score + MERGE_DEPTH ≥ blue_score(SP)`
    (a simplified version of Kaspa's bounded merge depth: blocks that are too old cannot be merged, so there is no need to pay for deep ancestor queries).
  - Reception time rule (only when receiving a new block): the timestamp is greater than the median of the previous 11 blocks on the selected chain (from `parents[0]` along selected parents) and at most 15 seconds ahead of local time.
  - BTC anchoring: the anchor height is not below the anchor height of any parent; the other anchoring rules (D-035, D-056) are unchanged per block.
- **Main chain (the virtual selected chain) and fork choice**: among all local blocks, the one with the largest `(blue_work, hash)` that is not disqualified (can be on the main chain) is the tip, and the path from it along selected parents to the genesis block is the main chain.
  A change of main chain (the tip's selected chain diverging from the old main chain at some block) is a reorg: the state rolls back to the fork point and the new main chain is executed block by block. `REORG_WINDOW` and the depth alarm are the same as D-041.
- **State and execution** (the header's `state_root` = the state root of the following):
  `state(B) = state(SP)` → move SP's pending sponsored fees to `0x08 ‖ SP` (D-017) → execute the entries of each block in `mergeset(B) \ {SP}` in turn (lenient mode) →
  execute B's own entries (strict mode). Along any selected chain, every block's entries execute exactly once: a chain block's as its own entries, every other block's inside the chain block that merges it.
  - **Strict mode** (a chain block's own entries): as now; violating any rule disqualifies the block.
  - **Lenient mode** (a merged block's entries): the environment (height, anchor and BTC view) is that of the chain block merging it; when a check that depends only on state and the BTC view (nonce, balance, fee debt,
    funding outputs and contributions, funding records, the conditions of unlock and fee-debt entries) fails, the entry is **skipped** (receipt `Skipped`, no state changed). Duplicate inclusion
    (the same op in two parallel blocks) and conflicts (different ops with the same nonce from the same account) therefore execute only the one ordered first. Path B token fees are credited directly to
    `0x08 ‖ merged block hash` and belong to the merged block's producer (its settlement carries these entries, and the D-038 unlock condition is judged by it as usual).
  - **State-independent rules are block validity** (invalid block, along with its descendants): version, chain ID, encoding lengths, fee mode matching the entry type, signatures (paths A, B), entry order,
    gas limits, `entries_root`, the settlement transaction (D-034, D-043), forced inclusion, the settlement backlog limit, merge limits and header rules. These checks need no execution state.
  - **Qualified**: `state_root` is verified only when a block becomes a main-chain candidate (by executing its selected chain). On a mismatch or a strict-mode failure, the block (and every block whose selected chain passes through it) is
    disqualified, but it remains a member of the DAG and can be merged by other blocks (its entries executing in lenient mode). Producers do not take a disqualified block as the selected parent.
- **Forced inclusion** (§7.4): block B includes all native transactions whose BTC height is in the window `(max(h(parents)), h(B)]` (`h` as in §7.4). The windows of parallel blocks can overlap;
  duplicate entries are skipped by nonce at merge time; along any selected chain, native transactions still execute in order of BTC height and in-block index, and none is missed.
- **Settlement** (§7.1, §7.2): every block still carries its own settlement transaction (the D-043 conditions are unchanged), and the payload's entries are all of this block's entries. The payload's headers: the last must be this block's
  header, and the others may be any headers (the producer's policy: headers in this block's past that have not yet landed on BTC and are not in any other known settlement payload, the newest by `blue_work`, at most
  `MAX_SETTLEMENT_HEADERS − 1` of them); headers are self-certifying through PoW, as in republish payloads. The former rule that "the payload must equal the canonical payload determined by the chain" (D-048) is dropped.
  - Broadcast timing (D-037): broadcast once the main-chain block that merges (or is) the block has `SETTLEMENT_DEPTH` confirmations. Before broadcasting, remove the input/output pairs of path A entries skipped in lenient mode
    (the producer rebuilds with its own key per §7.3; other nodes do not broadcast on its behalf a settlement containing skipped input pairs, and only rescue in the D-045 order, using only the executed input pairs).
    Thus when the same path A op is included by two parallel blocks, only the settlement of the block that executed it spends its funding input; should the other land on BTC first, the spending transaction kept the paired
    output at the same index, so per §8.4 no fee debt is recorded.
- **Settled prefix → settled closure** (D-038): `data(b)` is unchanged; `prefix(b) = max(data(b), max(prefix(parents)))`, 0 for the genesis block. Block b is settled at BTC height H
  if and only if `prefix(b) ≤ H` (the data of b and of its whole past has landed on BTC). Unlocking, BTC finality and the indexer all follow it. An op's BTC finality follows the main-chain block that executed it.
- **Settlement backlog limit** (D-046): B is any block in `past(N)` that carries a settlement transaction and still has no valid settlement as of N's anchor (no longer only ancestors on the selected chain).
- **Indexer** (§10.3): builds the DAG from the headers on BTC with the same GHOSTDAG, selects the main chain as the node does, and executes the main-chain blocks within the settled closure one by one (first executing
  the blocks it merges in lenient mode) and verifies `state_root`. A main-chain block whose `state_root` mismatches, that fails strict mode or that violates the backlog limit is disqualified: it is marked, the main chain is re-selected and processing continues
  (in an honest network such a block may still land on BTC through the settlement of a block merging it, so it can no longer stop per D-048); when the body of the block or of a block it merges is invalid (state-independent entry rules,
  forced inclusion mismatch), it still stops processing and raises an alarm per D-048 (M7.7).
  - Confirmation count (local policy, M7.8, found by the 5-hour soak run 36872025271): a main-chain block is executed only once it has `SETTLEMENT_DEPTH` confirmations on the indexer's main chain (the processable
    prefix = the lower of the settled closure and "main-chain tip height − `SETTLEMENT_DEPTH`"; the rollback decision uses the same prefix). Reason: when a merged parallel block Y carries a settlement transaction,
    the node broadcasts Y's settlement as usual once the main-chain block merging it has `SETTLEMENT_DEPTH` confirmations; if the main-chain block X at the same height carries no settlement transaction, its header and those of the empty blocks after it have to wait
    for the next block with a settlement transaction (or a republish, D-047) to land on BTC. While only Y is on BTC, it is the tip of the indexer's main chain, and if it were executed regardless, the indexer would temporarily differ from the node at this height
    (the soak found this at height 1048: the node's main-chain block was X, the indexer executed Y, and it is only corrected once X's header lands on BTC). Whenever the node broadcasts a settlement, its main chain is already
    `SETTLEMENT_DEPTH` blocks past that block, and the headers of those blocks will all land on BTC later, so waiting for them does not stall the indexer; the cost is that the indexer lags the node's settled prefix by
    `SETTLEMENT_DEPTH` blocks. When the lighter side produces more than `SETTLEMENT_DEPTH` consecutive blocks before being merged (a partition), the indexer may still follow that side first and then roll back and correct as usual,
    consistent with the node's assumption in the same situation (no deeper reorg occurs after a settlement is broadcast).
- **Sync** (§11.1, D-040): the protocol becomes `/ksana/sync/2` (implemented in M7.6; local-policy details at the end of this entry). `Status` gains `tips`: the DAG tips that
  can still be merged (at most 64); `tip_work` is the blue work of the main-chain tip. `GetBlocks { locator, after, max }`: the peer takes the first block F in the locator that is on its main chain and
  returns the known blocks not in `past(F) ∪ {F}`, in ascending `(blue_work, hash)` order (topological order; when `after` is given, only those with a key greater than it), at most `max` of them. We sync when the peer's blue work
  is greater or its tips contain blocks we do not have; blocks with missing parents go to the orphan pool, and the parents are fetched by hash from the source node.
- **Unchanged**: VmOp, entries, the settlement transaction structure, the state key space, gas, fee and fee-debt rules; in the SPEC, "parent" under the DAG means the selected parent (`parents[0]`),
  and "ancestors" means the past `past(B)`.
- **Scale and remaining limits**: the node and indexer keep the GHOSTDAG data in memory (a few hundred bytes per block, enough at devnet scale; mainnet needs Kaspa-style pruning, left for later).
  When a partition lasts until one side's blocks exceed `MAX_MERGESET_SIZE` or `MERGE_DEPTH`, the lighter side can no longer be merged, and the ops in its blocks return to the entry pool as in a reorg before.
- **Impact**: almost every crate: `ksana-primitives` (block header v2, parameters), `ksana-consensus` (GHOSTDAG, DAA, header rules), `ksana-execution`
  (lenient mode, merge execution), `ksana-btc` (payloads, settled closure, forced inclusion window), `ksana-node`, `ksana-p2p`, `ksana-indexer`, `ksana-rpc`,
  `ksana-cli`, `sdk/ts`, the shared vectors and the fuzzing seeds.
- **Node implementation (M7.5)**:
  - Main-chain tip: each DAG tip is walked along its selected chain to the main chain; when there is no disqualified block on the path, the candidate is the tip itself, otherwise it is the selected parent of the disqualified block
    on the path closest to the main chain (when that is not on the main chain); the candidate with the largest `(blue_work, hash)` is taken, and we switch only if it is strictly greater than the current tip. With equal blue work the larger hash wins; there is no "first received
    wins". If a switch fails midway (state root mismatch, strict-mode failure), the old main chain is restored, the failed block is recorded as disqualified (in memory; re-judged after a restart), and the next best is chosen.
  - Block template: the main-chain tip is the selected parent, and the other DAG tips are tried one by one in descending `(blue_work, hash)` order; those where the GHOSTDAG computation fails (merge set or merge depth
    over the limit) or that would change the selected parent are skipped, up to `MAX_BLOCK_PARENTS` in all. A lighter side over the limit is therefore no longer merged; when that side's nodes switch main chain, the ops in its blocks return to the entry pool
    to be included again by later blocks.
  - The other headers in the settlement payload (local policy): starting from the parents, look at the blocks in the past in descending blue work order (at most 1000), skipping blocks that carry a settlement transaction and not searching further into their
    past, collecting those whose header has not yet appeared on BTC, at most `MAX_SETTLEMENT_HEADERS − 1`, ordered by ascending blue work.
  - Settlement records and broadcasting: one record per block with a settlement transaction accepted by the main chain, its height being that of the main-chain block that accepted it (the D-037 confirmation count is computed from it). When a block has skipped
    path A entries, the settlement transaction the block carries is not broadcast; it is left to rescue settlement (D-045) to rebuild with the executed input/output pairs; the block's producer rebuilds immediately, and other nodes wait in the D-057
    order.
  - Op index: when an op appears in several blocks, the position where it executed is kept; when it appears only at skipped positions it is recorded as skipped (RPC status `skipped`, M7.8). The position records the height of the main-chain block
    that accepted the block, and BTC finality, the settlement's `settled` and so on are judged by it. An op skipped in a merged block returns to the entry pool while it is still valid (same as the ops of a disconnected block).
  - RPC (M7.8, `docs/RPC.md`): `ksana_getBlock` gains `accepted_height` and `merged`; a height identifier refers to a main-chain block, while a hash may be any block in the DAG.
    `skipped` is not a final status: `ksana-cli --wait`, the TS end-to-end scripts and the wallet demo page keep waiting when they see it (on a CLI timeout, if the last query still returned `skipped`, it
    is output).
- **Sync implementation (M7.6, local policy)**:
  - `Status.tips`: the DAG tips whose blue score + `MERGE_DEPTH` is not below the main-chain tip's, in descending `(blue_work, hash)` order, at most 64; the receiver looks only at the first 64.
    (D-104 item i: leaving out those anchored, or merging a block anchored, off the node's BTC best chain.)
  - The `GetBlocks` server omits the blocks below the point on F's selected chain where the blue score drops under `blue_score(F) − 2 × MERGE_DEPTH` (they can no longer be merged), pages by the DAG's sort key
    index, and searches `past(F)` only between that lower bound and F; when `after` is not a block known to us, it returns empty.
  - Session: after each Status query, when the peer's blue work is greater or its tips contain blocks we do not have (orphans count as not had), fetch GetBlocks in cursor-paged batches; then fetch by hash the peer's tips still missing after GetBlocks
    and the parents missing from orphans (at most 64 GetBlock per sync, 16 rounds). A block with unknown parents no longer interrupts the sync. When the previous sync yielded nothing,
    it is not repeated while neither the peer's tips nor the DAG tips change (except for `request_sync`).
  - After an isolation is reconnected, the heavier side's nodes learn the lighter side's tips through Status and fetch them, and the next block can merge them (`m4_network.rs`); a new node syncing from the genesis block gets the same
    DAG, main chain and state.

## Numbering of D-064 to D-071

This range of numbers is used by the parallel web wallet and block explorer work (branch `claude/brave-carson-haoi9i`, based on `m7`, from 2026-10-01; moved on top of `main` on 2026-10-04
and rewritten per D-078 and D-092; see the individual entries). The `m8` branch initially recorded the SPV fix, the EVM contract model and KSA issuance as D-064 to D-066, which collided with these, and renamed them on 2026-10-02 to
D-072 to D-074. Earlier commit messages (from `af2430b` to the M9 commits) still use the old numbers: old D-064 = D-072, old D-065 = D-073, old D-066 = D-074.

## D-064 Web wallet and block explorer (`web/`) and their dependencies (parallel work, for the record; revised per main on 2026-10-04)

- **Problem**: the repository layout in SPEC §12 has no web front end; `sdk/ts/examples/wallet-demo` (D-055) is only a minimal page that verifies a single path, with no block explorer, and it does not explain to users
  the two kinds of confirmation in §10.4 or the trade-off between the two paths.
- **Decision**: add `web/` (Vite + React + TypeScript), the explorer and wallet in one, with the design notes in `docs/WEB_DESIGN.md`. There are two data sources, and the pages do not distinguish between them:
  - an in-browser devnet simulator (`web/src/chain/sim/`) whose rules follow `main`: GHOSTDAG merging and skipping (D-063), paths B / C (D-078), the commit / reveal pair of
    settlement transactions and payload compression (D-075, D-079–D-081), settlement batching and carried bodies (D-077), two-speed difficulty and empty-block streaks (D-084, D-090), cumulative unlock and block rewards
    (D-074, D-079), block share (D-089), BTC-anchored finality (D-082), the example exchange contract's lock → claim → release (D-085, D-086), for online previews and UI tests;
  - node JSON-RPC (`web/src/chain/rpc/`, reading `ksana-node` and optionally `ksana-indexer` through the `@ksana/sdk` client), for connecting to a real devnet / signet.
    The wallet connects to Unisat and Xverse through the SDK adapters: for path B the wallet makes the BIP322 signature; for path C and top-ups the wallet signs a PSBT, and the BTC transaction is broadcast through bitcoind RPC (a same-origin proxy)
    or by the wallet; when neither is available, the raw transaction is shown for the user to broadcast themselves. Local regtest additionally has a test private-key wallet.
  - The simulator is for demonstration only and never pretends to be a real network: the top status bar and the footer always state the data source; quantities the node cannot provide are shown as "—" with the reason noted.
- **Hidden protocol choices**: the UI does not let the user choose the path or the signing method. Sending defaults to path B (paid in KSA, BIP322 simple signature); only when the account has no KSA, or the node rejects the op /
  it is still not included after several blocks, does the UI propose path C (carrying the op in a BTC transaction, the signature bound to the carrying transaction's first input per D-086) and explain the two outcomes. Exchanging BTC for KSA goes through
  the top-up page (a lock in the example exchange contract, D-085).
- **Integration**: `crates/ksana-node/tests/web_devnet.rs` (a test ignored by default) starts two producing nodes and an indexer locally with the mock bitcoind from the node tests, needing neither docker
  nor bitcoind, and sends a confirmed UTXO to each of alice's and bob's account addresses (for path C and top-ups); the node responses used by the web replay tests were recorded on it
  (`web/src/chain/rpc/fixtures/devnet.json`).
- **Dependencies and rationale**:
  - `react`, `react-dom`: the UI framework; `vite`, `@vitejs/plugin-react`, `typescript`, `vitest`: build, type checking and unit tests.
  - `@ksana/sdk` (`file:../sdk/ts`, this repository): account ↔ address, Borsh encoding / decoding, the RPC client and D-007 JSON parsing, construction of native transactions and top-ups, wallet adapters,
    keeping the formats consistent with the node; `web/` therefore requires the SDK to be built before installing (`(cd sdk/ts && npm ci && npm run build)`). Parsing of the Taproot envelope (D-081) is in
    `web/src/chain/rpc/convert.ts`, following the rules of `ksana-btc/src/envelope.rs`.
  - `@fontsource/ibm-plex-mono`: the monospace font for data; the serif (Song) heading font is a subset of Noto Serif SC SemiBold (`web/scripts/subset-font.py`, about 160 KiB,
    SIL OFL 1.1, license in `web/src/assets/fonts/OFL.txt`). The fonts are bundled with the app; the wallet makes no requests to third-party font services, so as not to leak usage.
  - `uqr`: the receive QR code, zero dependencies.
- **Impact**: no effect on consensus code or the node (`web_devnet.rs` is only a test). Versions are locked in `web/package-lock.json`; CI has a separate `web` workflow (build the SDK first, then type-check,
  unit-test, build).

## D-065 Account public keys have no checksum (web) -- resolved by D-092

- **Problem** (2026-10-01): since D-052 an account is an x-only public key: a transfer can only be filled in as 64 hex chars, the account cannot be derived from a BTC address, hex has no checksum, and a single mistyped digit most likely still gives
  a valid public key, so assets end up in an account whose private key nobody holds. The suggestion at the time was to define a text encoding with a checksum for accounts.
- **Outcome**: D-092 defines the account as the 32-byte output key of a Taproot address; the bech32m address itself is an account encoding with a checksum and a network prefix. The web page shows a single identifier
  (the account address), the receive page gives the address and a QR code; everywhere an account is entered, either the address or 64 hex chars is accepted (SDK `parseAccount`), and an address whose network does not match the data source is rejected;
  contract IDs remain 64 hex chars. P2WPKH (`bc1q`) addresses are no longer accounts. The account seal is kept for quick checking.
- **Status**: resolved (D-092).

## D-066 Display precision of KSA (web) -- 8 decimals per SPEC §8.6

- **Problem** (2026-10-01): the protocol did not define asset precision at the time, and the web page and the CLI both displayed integer units.
- **Outcome**: since D-074 KSA is a built-in asset with 8 decimals in its metadata (SPEC §8.6), and `ksana_getAsset` returns `decimals`. The web page displays and accepts KSA with 8 decimal places
  (`KSA_DECIMALS = 8`), other assets per the decimals from `ksana_getAsset`; `--amount` of `ksana-cli` is still in base units, and the UI notes the number of base units next to the amount, to avoid
  the same number differing by a factor of 10⁸ between the two places.
- **Status**: resolved.

## D-067 Mining blocks back to back on regtest briefly triggers the settlement backlog limit (web, record)

- **Problem**: the backlog limit of §6.4 is counted in BTC blocks. When regtest mines several blocks at once (e.g. `generatetoaddress 6`), the settlements of the last few fast-lane blocks are still waiting for
  `SETTLEMENT_DEPTH` before they can be broadcast (with settlement batching on, they also wait for the in-flight one to confirm, D-077), yet the anchor of new blocks jumps forward by 6 blocks at once, and the next fast-lane block may not be allowed to include
  fast-path entries; it recovers automatically once the settlements land on BTC. This matches the letter of the rule, but is not the situation the rule is meant to guard against (data not reaching BTC for a long time).
- **Suggestion**: when integration tests mine BTC blocks, leave a gap of at least `SETTLEMENT_DEPTH` fast-lane blocks, or assert this behavior explicitly in the relevant tests. On mainnet, with one block every 10 minutes, it does not occur.
- **Current approach**: the web simulator follows the letter of the rule, and the regtest console has a note next to "mine 6 blocks".

## D-068 Ops dropped from the entry pool cannot be queried (web, closed out 2026-10-05: the web page's approach stays, the node keeping the reason for dropped ops is listed as an RPC improvement for after mainnet, D-096)

- **Problem**: the statuses of `ksana_getOpStatus` are `pending`, `fast_confirmed`, `failed`, `btc_final`, `skipped` (docs/RPC.md). Once a path B op
  has been dropped from the entry pool because a path C op with the same nonce was force-included first, the node no longer knows it and returns `-32004`. The wallet cannot tell "the node never received it" from "superseded by another
  op of the same account", and the latter is exactly the scenario §15 requires (the same nonce submitted via both path B and C: the one included first takes effect, the other is recorded as failed).
- **Suggestion**: the node keeps dropped ops and the reason for a while (e.g. `dropped`, with `nonce_used`), and `ksana_getOpStatus` reports them as such.
- **Current approach**: the web simulator marks them "dropped" and gives the reason; when connected to a node, a path B op submitted from this page is marked "dropped" only after two consecutive queries return `-32004` (reverted if it reappears in the entry pool),
  with the explanation "the node no longer knows this op: it may have been dropped from the entry pool"; a path C op is unknown to the node anyway until its carrying transaction lands on BTC, so it is not marked this way but shown by the confirmation progress of the
  BTC transaction; when a query for an unknown op_id gets `-32004`, both possibilities are explained.
- **Status**: closed out 2026-10-05 (D-096): the web page's approach stays; the node keeping the reason for dropped ops (e.g. `dropped`, with `nonce_used`) is an RPC improvement and does not change consensus,
  deferred to after mainnet (SPEC §17.3).

## D-069 Read-only queries the explorer needs (web, suggestion; the listing of an account's operations added by D-111)

- **Problem**: the existing RPC is enough to browse block by block (fetch main-chain blocks by height, `merged` brings along the parallel blocks), to query op status and account balances, and `ksana_getChainInfo` (D-076) gives the tip and
  the BTC-final tip, but the block explorer and the wallet also need: the entry pool listing, ops listed by account, balances in the BTC-final state (§10.4 requires distinguishing the two kinds of confirmation,
  `ksana_getAccount` only gives the latest state), and data from the BTC side (block time, input amounts, fees, UTXOs).
- **Current approach** (`web/src/chain/rpc/`): subscribe to new blocks, walk the main chain back along the selected parents, and fetch the most recent stretch with the parallel blocks it merges and their settlement status; for blocks whose bodies must land on BTC and for the blocks that carry them,
  `ksana_getSettlement` is fetched (`carrier` points to the carrying block, D-077); where a header landed on BTC is inferred from the settlement transactions whose payload contained it; the settled and BTC-final heights
  are the highest acceptance heights with `settled` / `btc_final` true, probed by bisecting over heights when the boundary itself lies below the cache (both are prefixes on the main chain); for the balance split, the background fetches another
  40 blocks downward each time until the BTC-final boundary is covered (on mainnet this may be several hundred blocks, which is exactly why an `at` parameter is needed); the entry pool lists only the ops submitted from this page; `ksana_getOpStatus`
  returns only the status, not the content, for ops in the entry pool, and the page says so when details are unavailable. The node does not provide the BTC-side figures: when a bitcoind RPC is configured (same-origin proxy, see D-070), the
  BTC tip, UTXOs and fee rates are taken from bitcoind, and path C and top-up transactions are broadcast through it; when none is configured, "—" is shown and labeled as such.
- **Suggestion**: add `ksana_getMempool`, `ksana_getAccountOps` and an `at` parameter for `ksana_getAccount` (or directly return the balance in the BTC-final state),
  and have `ksana_getOpStatus` include the VmOp for ops in the entry pool; or let the indexer provide these read-only interfaces. All are read-only queries and do not change consensus.
- **Status**: 2026-10-14, D-111 adds `ksana_getAccountOps` to the node, with `ksana_getLatestOps`, and the page's history lists use them; the rest of the suggestion stands.

## D-070 The indexer has no cross-origin setting; bitcoind goes through a same-origin proxy (web, suggestion)

- **Problem**: `ksana-node` has `--rpc-cors` (D-055), `ksana-indexer` does not. A browser page therefore cannot call `indexer_status` / `indexer_stateRoot` directly:
  the web page's "Verify" page needs to compare the state_root recomputed by the indexer and can only reach the indexer through a same-origin reverse proxy. The bitcoind RPC likewise has no cross-origin headers and requires credentials, which cannot be put into the browser.
- **Current approach**: during development `web/` proxies all three through Vite (`KSANA_RPC=… KSANA_INDEXER=… KSANA_BTC=http://user:pass@127.0.0.1:18443 npm run dev`,
  with the page configured with `/rpc`, `/indexer`, `/btc`; the bitcoind credentials stay on the proxy side and are invisible to the page); in deployment, the server hosting the page reverse-proxies them. With no indexer connected, the "Verify" page
  explains why and the status bar shows "No indexer"; with no bitcoind connected, path C and top-up fall back to the wallet's UTXO and broadcast interfaces (Unisat), and with neither, the user fills in UTXOs by hand and
  broadcasts on their own.
- **Suggestion**: add to `ksana-indexer` the same `--rpc-cors` option as the node (reusing `ksana_rpc::Cors`), still disallowed by default. This only affects HTTP headers and does not change consensus.

## D-071 Multilingual web page and trimmed copy (web, record; since D-115 the page opens in English whatever the browser's languages, and English ships in the bundle)

- **Problem**: the web page's copy exists only in Simplified Chinese and has too much explanatory text (spec section numbers, decision numbers, RPC method names and rule derivations written into the user interface); overseas users cannot use it,
  and domestic users find it verbose. Once multilingual, every sentence has to be translated and maintained in every language, and the more text, the higher the cost.
- **Approach**:
  - First trim per the new copy guidelines (`docs/WEB_DESIGN.md` §8): write a sentence only where a decision is needed, there is a risk, or something went wrong, at most one sentence per place; the user interface carries no spec or decision numbers and no
    RPC method names; each concept is explained once across the whole site. Then extract all user-visible text into strings.
  - First batch of languages: Simplified Chinese (the source language), English, Traditional Chinese, Japanese, Korean, Spanish. The strings are typed TypeScript objects (`web/src/i18n/<language>/`),
    the structure of Simplified Chinese is the type, and another language with a missing string or wrong parameters fails the type check; no i18n dependency is introduced (the scale is small, and type checking is more reliable than looking up keys at run time).
    Simplified Chinese ships in the bundle, other languages are loaded on demand. The language is taken from `?lang=` in the address bar, the previous choice, then the browser language, and English when none is supported. Switching languages remounts the interface.
  - Numbers, timestamps and hashes are written the same way in every language (comma thousands separators, decimal point, 24-hour clock): amounts must paste as-is into the command line and RPC, and numbers in screenshots do not change with the language.
  - Text returned by the node (error messages in receipts, RPC errors) is shown as is: it comes from ksana-node, and the web page only classifies it by error code and prefix, without translating the node's original text.
  - Text produced by the simulator is recorded as "string + parameters" and only rendered in the current language at display time; the names of contracts deployed by the user on the simulated chain are on-chain data, identified in English, and do not change with the language.
  - Fonts: Han characters have different glyph forms in Chinese, Japanese and Korean (same code points), so headings and body text in Traditional Chinese, Japanese and Korean use the system fonts of their own language instead of borrowing the Simplified Chinese Song-typeface subset;
    English and Spanish headings use the Latin letters from that same subset (the subset includes the full Latin-1 range). In English and Spanish, the Han character for "eternity" on the vermilion seal is replaced with a meander-pattern seal (since D-099 every language uses the meander-pattern seal).
- **Status**: record; the six languages are done. Adding a language only takes creating a new directory, implementing all namespaces and its `name.ts` (the language's own name, D-099), and registering both in `web/src/i18n/index.ts`;
  terminology follows the term list in `docs/WEB_DESIGN.md` §8.3.

## D-072 The contract SDK's BTC transaction inclusion proof pins the Merkle tree depth (2026-10-02, review comments forwarded by the project owner; not a consensus rule)

- **Problem**: SPEC §9.4 only says "verify that a BTC transaction has been confirmed using the Merkle proof tools provided by the SDK". The preimage of an internal node of the Bitcoin Merkle tree is exactly
  64 bytes (two 32-byte hashes) and cannot be distinguished from a 64-byte transaction (CVE-2017-12842). M1's `btc::verify_tx_inclusion` only rejected a 64-byte
  `raw_tx`, which blocks "passing off an internal node as a transaction" (a proof shallower than the real leaves) but not the other direction: if the block contains a real 64-byte transaction
  `T = L ‖ txid(F)`, then treating T as an internal node gives the never-happened transaction F a branch one level deeper than the real leaves that computes to the block's Merkle root, and F itself
  is not 64 bytes. This was reproduced on the original SDK with a temporary test (the forged proof was accepted). The attacker has to get miners to include a specially crafted 64-byte transaction (relay policy
  does not relay it, but it is currently valid under consensus) and do a large brute-force search, so the bar is high, but contracts of the "BTC payment triggers issuance" kind would mint assets for the forged payment.
- **Decision** (the tBTC v2 approach): `verify_tx_inclusion(header, raw_tx, index, branch, coinbase_tx, coinbase_branch)` additionally requires the proof of the same block's
  coinbase transaction: `coinbase_tx` is the coinbase (serialized without witness, exactly one input whose previous output is null: all-zero transaction ID, index
  `0xffffffff`, `btc::is_coinbase`), its branch at position 0 computes to the same root, and the two branches have equal length. The Bitcoin Merkle tree duplicates the last
  node on odd levels, so all transactions sit at the same depth; the coinbase is always at position 0 and is a real leaf, so it fixes the depth, and the grafted forged branch, one level deeper, is rejected.
  Forging a coinbase proof that is likewise one level deeper would require the coinbase itself to be 64 bytes and the forged transaction's txid to equal its first half (the version, `0x01` and 27
  zero bytes), which is infeasible. The 64-byte check is kept.
- **Example contract**: `claim` of `btc-payment-trigger` becomes
  `(height, raw_tx, tx_index, branch, coinbase_tx, coinbase_branch, vout, beneficiary)`.
- **Tests**: SDK unit tests (the grafted branch does compute to the same root but is rejected; coinbase recognition; a non-coinbase at position 0, a tampered branch, or
  unequal lengths are all rejected), real mainnet block data, the same scenario in the example contract, and the regtest case now submits the real block's coinbase and its branch.
- **Impact**: `ksana-contract-sdk` (API change), the example contracts and their tests, `tests/integration/tests/m1_regtest.rs`. Consensus is not involved; the node and
  the indexer do not use SPV proofs.

## D-073 Contract model aligned with the EVM (M8, project owner's decision; revises D-012 to D-014, D-016, D-021 and SPEC §9.2, §9.3)

- **Problem**: the project owner (2026-10-02): "Call should carry funds... make the structure like the EVM, a bit freer". M1's contract model was designed for least privilege (D-016):
  calls could not attach funds, and built-in assets (KSA and assets issued by contracts) could not be transferred into a contract under the caller's name; when a called contract failed, the whole op failed and could not be caught;
  contracts could not create contracts; deployment required an acyclic call graph (D-014), so Rust contracts could not use recursion, trait objects or `core::fmt` (D-021); and there was no block timestamp.
- **Decision** (still WASM contracts, no EVM bytecode compatibility; the non-goals of SPEC §1 are unchanged):
  1. **Calls carry attached funds** (a multi-asset `msg.value`): `Action::Call` and `Action::Deploy` gain `funds: Vec<Funds { asset, amount }>`,
     at most `MAX_CALL_FUNDS = 16` items, asset IDs strictly ascending and amounts greater than 0, otherwise the op fails. At execution, each item is first transferred from the signing account to the contract (insufficient balance fails the op),
     then the contract is called; if the call fails, the transfers are rolled back with it (§9.2). The contract reads the funds attached to the current call with the host function `call_funds`.
     `VMOP_VERSION` is raised to 2 (an encoding change; the shared vectors and the TS SDK are updated in step).
  2. **Contract-to-contract calls carry funds**: `call_contract` gains a `funds` parameter, paid out of the calling contract's own balance. The baseline of D-016 is unchanged: a contract can only move its own balance,
     and user assets can only be moved by an op the user signed (attached funds are exactly such a transfer authorized by the user's signature).
  3. **Catchable sub-calls**: new `try_call_contract` (may attach funds, may set a gas limit): when the called contract fails (revert, trap, out of gas, rule violation),
     only that call level is rolled back (including its transfers, storage writes and events), a status code and the revert data are returned, and the caller continues. `call_contract` still fails the whole op
     on failure. The sub-call's gas limit is exact (the same unit of protection as D-020).
  4. **Delegate call**: new `delegate_call`: executes another contract's code in the current contract's context (storage, balance, caller and attached funds), for proxies and upgradeable contracts.
  5. **Contracts creating contracts**: new `create_contract(code_hash, salt, init_args, funds)`: the code must already be on chain (stored by some earlier `Deploy`),
     `contract_id = sha256(creator ‖ salt ‖ code_hash)` (like CREATE2; its preimage length differs from that of a user deployment, `sha256(deployer ‖ nonce ‖ code_hash)`,
     so they cannot collide), and `init` is called with the creator as `caller`; it fails if the address already exists or the code does not exist.
  6. **Recursion and indirect calls allowed**: deployment no longer requires an acyclic call graph; instead the stack depth is metered deterministically at run time: before compilation, each function gets a counter that is incremented on entry and decremented on exit
     (accumulated into a newly added global according to the stack frame estimation formula of D-014), and exceeding `STACK_BUDGET` traps. The metering depends only on the bytecode and is architecture-independent;
     the limit per contract instance is the same as in D-014, and the runtime safety margin is unchanged. The stored code is still the original bytecode (`code_hash` unchanged); the instrumentation is part of the consensus rules.
     Rust contracts may use recursion, trait objects and `core::fmt` (the restriction of D-021 is lifted). The instrumentation uses `wasm-encoder` (from the same wasm-tools family as the already used `wasmparser`,
     same version, already in the dependency tree).
  7. **Execution environment**: new `block_timestamp` (the timestamp of the main-chain block executing the op), `chain_id`, `gas_left`; new `asset_burn` (burns assets held by this contract,
     reducing the total supply accordingly). No `tx.origin` (a recognized anti-pattern in the EVM).
- **gas** (consensus parameters, `docs/ABI.md`): 10,000 per attached funds item; `try_call_contract` same as `call_contract`; `delegate_call` same as `call_contract`;
  `create_contract` 100,000 + 1/B for loading the code + the execution of `init`; `block_timestamp` / `chain_id` / `gas_left` 100 each; `asset_burn` 10,000.
- **Impact**: `ksana-primitives` (`Action`, `Funds`, `VMOP_VERSION`), `ksana-vm` (host functions, instrumentation, stack checks), `ksana-execution` (attached funds,
  nested rollback, contract creation, block environment), `ksana-contract-sdk` (API and mock host), the example contracts (AMM support for built-in assets, factory), TS SDK, CLI, RPC,
  the shared vectors, and the fixed bytecode and state roots of the determinism test.
- **Implementation (M8.1 attached funds)**: the rule checks, gas (10,000 per item, charged before the contract executes) and transfers for top-level attached funds all live in the execution layer; a rule violation, insufficient gas or
  insufficient balance only fails the op; the VM only records the funds attached at the current level. The `funds` parameter of `call_contract` is a Borsh-encoded `Vec<Funds>`, length 0 meaning none attached,
  checked against the length limit (772 bytes) before decoding and required to be fully consumed; the transfer out of the calling contract happens after the callee's code is loaded and before the callee is entered. Contract SDK: `call_funds` /
  `received` / `call_with_funds` (the SDK does not sort on the contract's behalf; out of order traps); TS SDK: `Funds`, `checkFunds` / `sortFunds`; CLI: `--fund
  [<asset ID>:]<amount>` (repeatable, sorted automatically); `funds` of `ksana_call`. The execution layer tests use the test contract `contracts/testing/probe` (which exposes the host functions
  as methods; the later M8 steps reuse it).
- **Implementation (M8.2 catchable calls and delegate calls)**: the execution layer's state buffer supports nested checkpoints (the undo log is only recorded while a checkpoint exists; events are truncated by count),
  and `Host` gains `checkpoint` / `revert_checkpoint` / `release_checkpoint`. The result of `try_call_contract` is Borsh `(status, data)`,
  distinguishing only success, revert and other failures (the specific failure reason is not exposed to the contract and does not become part of consensus). The sub-call's gas limit follows the EVM's 63/64 rule
  (EIP-150); on revert the unused gas is refunded, other failures consume the whole limit (same as the EVM; this also keeps the result independent of where wasmtime's internal fuel accounting
  stands at the time of a trap). The caller's own errors (input, method name, attached funds rules) cannot be caught. `delegate_call` attaches no funds and fails the whole op on failure (consistent with `call_contract`;
  when catching is needed, the caller wraps it in another `try_call_contract`), and reuses the current frame's `caller` and attached funds without transferring again. SDK: `try_call` /
  `try_call_with` / `delegate_call` / `CallError`; the mock host does not meter gas.
- **Implementation (M8.3 environment and creation)**: `BlockEnv` gains `timestamp` (the block header's timestamp); the ops of a merged block execute under the main-chain block that merges it and see the main-chain block's
  timestamp (consistent with the height, D-063); `ksana_call` simulation uses the tip's timestamp. `gas_left` charges its own cost first and then reads the remainder (wasmtime has already saved the
  fuel before the host function call, so the reading is exact). `asset_burn` burns from the balance of the current contract (the delegator in a delegate call); the op fails if the asset does not exist or the balance is insufficient. `create_contract`
  carries no code bytes and only references a code hash already on chain (avoiding re-transmitting and re-validating code within a call); an address collision, missing code, or a failing `init` all fail the whole op (consistent with
  `call_contract`; when fault tolerance is needed, put the creation into a method invoked via `try_call_contract`); the deployer in the contract metadata is recorded as the creator contract.
  SDK: `block_timestamp` / `chain_id` / `gas_left` / `asset_burn` / `create_contract` / `created_contract_id`; the mock host's
  contract registry rolls back with a failed op, and code is registered by `deploy` and `register_code`.
- **Implementation (M8.4 runtime stack metering)**: the deployment check keeps the rules on the feature set, imports, exports, memory and tables, and estimates a stack frame for each function; the call graph is no longer checked.
  Before compilation (once per code hash, result cached) the module is rewritten with wasm-encoder's reencode: an i32 global is appended; when a wrapper block with multi-value results is needed, a
  `() -> (results)` type is appended at the end of the type section; a module without a global section gets one added where it belongs. Each function entry adds the frame's estimate and compares it with `STACK_BUDGET` (exceeding it is
  `unreachable`), the function body is wrapped in a block with the same result type as the function (`return` becomes a `br` to that block; a `br` / `br_table` targeting the function label naturally lands on
  that block), and the frame is subtracted after the block. After a trap the instance is no longer used, so the counter need not be restored. The inserted instructions are charged gas like ordinary instructions, so every contract's gas usage increases slightly
  (state roots are unchanged; the golden roots of the determinism test need no update). Tests: the recursion depth limit is the same under both engine configurations (about 2 330 levels × 112 bytes), mutual recursion and
  indirect recursion through a table, a single stack frame over the budget traps at entry, the various exits (`return`, `br` / `br_table` to the function label, multi-value returns, the contract's own globals)
  leave no accumulated count after 100,000 calls; the Rust test contract uses recursion, trait objects and `format!`.
- **Implementation (M8.5 examples)**: the two sides of the AMM example become `Side::Token(contract ID)` / `Side::Asset(asset ID)`: the built-in asset side is funded with the funds attached to the call
  (the attached amount must equal the amount in the arguments exactly, and the part left unused by the ratio is refunded; attaching assets outside the pair, attaching funds for the output side, or attaching funds to `remove_liquidity`
  all revert, so that assets do not get stuck in the contract), and pays out with `asset_transfer`; the token side is still approve / `transfer_from`. New example
  `amm-factory`: it records the AMM's code hash; `create_pair` uses as the salt the sha256 of the Borsh encoding of the two sides in canonical order (by ID, then by kind)
  and calls `create_contract`; the same pair can only be created once and its address can be computed in advance. The determinism test adds blocks 11 to 13 (probe's fixed bytecode),
  covering attached funds (including path C), catchable calls (revert and sub-call out of gas), delegate calls, contract creation, burning, recursion and failure from exceeding the stack budget,
  `core::fmt` and dynamic dispatch; the golden roots of the first 10 blocks are unchanged.

## D-074 KSA issuance and block rewards (M9, project owner's decision; values closed out 2026-10-05: devnet / signet keep the placeholder values, mainnet per D-089, D-096; mainnet's halving periods per D-105; revises SPEC §8.5, §8.6)

- **Problem**: SPEC §8.6 and §17: the issuance and distribution of the project token are to be decided; devnet has only the genesis allocation and no block rewards. The project owner (2026-10-02):
  "build token issuance and block rewards in; they need to be tested".
- **Decision** (the values are placeholders, kept for devnet / signet; the mainnet values follow D-089, closed out 2026-10-05, D-096; all are per-network consensus parameters):
  1. **Max supply** `KSA_MAX_SUPPLY` = 1 billion KSA (8 decimals, i.e. 10^17 base units), the cap on the sum of the genesis allocation and all block rewards.
     A genesis config whose total allocation exceeds `KSA_MAX_SUPPLY − max total rewards` is invalid.
  2. **Reward schedule**: by the rewarded block's DAA score `d` (`daa_score = |past(B)|`, D-063, roughly the number of blocks since genesis), halved once every `REWARD_HALVING_INTERVAL`
     blocks: `reward(d) = BLOCK_REWARD >> ⌊d / REWARD_HALVING_INTERVAL⌋` (a right shift of 128 bits or more gives 0). Placeholders: the interval is the number of blocks in 4 years (1 460 days)
     (25 228 800 at one block per 5 seconds), `BLOCK_REWARD` = 9.9 KSA, and the total rewards do not exceed `2 × BLOCK_REWARD × interval` ≈ 499.5 million KSA (half of the cap).
     When M10 adjusts the block interval, these two parameters are rescaled by seconds, keeping the issuance per second and the halving time unchanged. They do not track the actual block count: the DAA score itself grows with time.
  3. **Which blocks are rewarded**: while executing along the main chain, each main-chain block credits one reward for itself and one for each **blue** merged block in its merge set (by their own DAA scores);
     red blocks get no reward (as in Kaspa: red blocks are parallel blocks beyond the network delay and may come from an attacker). Along any selected chain, each block appears exactly once, either as a main-chain block or in the merge set of one
     main-chain block, so it receives at most one reward; a disqualified block that is merged as blue is rewarded all the same.
  4. **Rewards are locked first** (the principle of §8.5: income only once the data is on BTC): the reward and the path B sponsored fees go into the same locked record -- the main-chain block's own goes into the pending record,
     moved to `0x08 ‖ hash` by the next block (D-017), and a merged block's goes straight into `0x08 ‖ hash` -- belonging to that block's producer. The total supply of KSA (asset metadata) increases when credited.
     Unlocking is still done by an `Unlock` entry once the block enters the settled closure. Blocks without a settlement transaction (D-043: empty blocks and blocks with only forced-inclusion entries) now also have locked records:
     when the `settlement_txid` of `Unlock` is 32 zero bytes, only that the block has entered the settled closure is checked (the settled closure itself requires that some valid settlement of each block carrying a settlement transaction is confirmed,
     so this is a sufficient condition for any block).
   - **Revision (at implementation, cumulative unlock)**: once every block has a locked record, one `Unlock` entry per block means every fast-lane block carries 65 extra bytes on BTC (at one block per second,
     about 600 entries per BTC block), so unlocking becomes **cumulative**, with the entry format unchanged:
     - When a locked record becomes final (for a main-chain block, after its pending record is moved to `0x08 ‖ hash` at the start of the next block; for a merged block, after its entries have executed), it is appended in order to
       the **pending-unlock queue**: `0x09` → `UnlockQueue { head: u64, tail: u64 }`, `0x0A ‖ sequence number (u64 big-endian)` → block hash, with elements `[head, tail)`.
       The queue order is the acceptance order: acceptance heights are monotonically non-decreasing.
     - `Unlock { block_hash: X, .. }`: X must have a locked record (not the all-zero pending record), the settlement check is as above (an all-zero txid checks the settled closure, otherwise X's
       settlement with that txid is checked); then records are unlocked in order from the queue head up to and including X: each record's amount is added to its producer's KSA balance, and the record and queue element are deleted. Once X has entered the settled
       closure, the blocks ahead of it have acceptance heights no higher and have all entered too (the settled prefix is monotonic in height), so only X needs checking. If the records up to X exceed
       `MAX_UNLOCK_BATCH` (a parameter, 1 024 on devnet), the entry is invalid (unlock earlier blocks first).
     - At most one `Unlock` entry per block (a state-independent entry rule): one entry can unlock a whole segment, and more would only amplify the execution load.
     - Records are deleted after unlocking (every block has a block reward, so keeping unlocked records would let the state grow without bound with the block count); `LockedFee` drops the `unlocked` field,
       and `locked_fees` of `ksana_getAccount` lists only records not yet unlocked (`unlocked` removed). A deleted record cannot be unlocked again; the same income is unlocked only once.
     - Anchor-invalid blocks (D-056) are replayed by the X written in the block, as before; if the execution layer unlocked automatically instead, how far to unlock would depend on the anchor chain and could not be reproduced
       after the anchor block is reorged away, so the entry still states it.
     - The node's block production policy: find the settled main-chain height under the anchored view, take from the queue head at most `MAX_UNLOCK_BATCH` blocks whose acceptance height does not exceed it, and use
       the last of them as X, with `settlement_txid` all zeros.
  5. **Interfaces**: the execution layer's `BlockEnv` gains `daa_score`, and `MergedBlock` gains `blue` and `daa_score` (supplied by the node and the indexer from GHOSTDAG data).
     RPC gains `ksana_getAsset` (name, decimals, total supply, issuer).
- **Impact**: `ksana-primitives` (parameters), `ksana-execution` (crediting rewards, unlocking with an all-zero txid), `ksana-btc` (the tracker answers an all-zero txid by the settled closure),
  the node (writing unlock entries for blocks without a settlement transaction when producing blocks), the indexer, RPC, TS SDK, CLI, docs and tests. State roots change accordingly (each block's locked record and the KSA total supply).
- **Implementation notes (M9)**:
  - Parameters: `Params::block_reward_at`, `max_total_rewards`, `max_genesis_supply`, and `MAX_UNLOCK_BATCH` as `max_unlock_batch`; the TS SDK's
    `DEVNET` carries the three issuance parameters and `blockRewardAt`. The genesis config file checks the allocation cap when generating the genesis block header (`GenesisConfig::check_supply`).
  - Execution layer: `BlockBuilder::new` first moves the parent block's pending record and enqueues it, then credits this block's reward by `BlockEnv.daa_score`; `merge` first credits a blue block's reward,
    executes its entries, and finally enqueues its record. No record is created when the reward is 0 (after 128 halvings, or with the parameter at 0). Both `check_entries_static` and
    `BlockBuilder::push` check that each block has at most one unlock entry.
  - `ksana-btc`: `settled_height` (the highest main-chain height that has entered the settled closure at the anchor height) serves the node's block production policy; `accepted_height` now searches backward
    from the tip (each block is accepted exactly once along the main chain, so the result is unchanged). `AnchoredView::block_settled` and `settlement_confirmed` judge the settled prefix the same way,
    and anchor-invalid historical blocks (D-056) are likewise judged by the current best chain.
  - The node and the indexer take `daa_score` and the blue / red coloring of the merge set from GHOSTDAG data; read-only RPC calls use the tip's `daa_score + 1` (rewards are not involved).
  - Tests: `ksana-execution/tests/rewards.rs` (main-chain blocks, blue / red merged blocks, halving, conservation, cumulative unlock, genesis cap) and `unlock.rs` (queue order,
    batch limit, all-zero txid, one per block), `ksana-node/tests/m9_rewards.rs` (the node writes cumulative unlock entries by the settled closure; an empty block's header lands on BTC with the
    settlement of the unlocking block and is unlocked next; KSA conservation; `ksana_getAsset`); the existing node, indexer and regtest tests are updated for rewards (the indexer's and the node's state roots still match block by block).
    `Chain::new` of the execution layer test harness uses parameters without rewards (`NO_REWARD`), so tests that only look at sponsored fees are unaffected by rewards; the golden state roots of the determinism test are updated accordingly.

- **Revision (2026-10-02, supply cap)**: while tidying up the pending-confirmation decisions it was found that rewards are computed from each block's own DAA score (`|past(B)|`), and parallel blue blocks can have the same
  DAA score, so the sum of rewards paid per block may slightly exceed `max_total_rewards` (the upper bound of the sum over DAA scores; it is only about `interval × popcount(BLOCK_REWARD)` base units
  wider than the exact series sum, about 17.7 KSA on devnet), so the cap on the KSA total supply was not hard. When crediting a reward, the execution layer now limits it to `KSA_MAX_SUPPLY − current total supply`
  (`credit_reward`; the excess is not issued and creates no record), so the total supply never exceeds the cap. Far from the cap, behavior is unchanged (the golden state roots of the determinism test are unchanged).
  Test: `rewards_stop_at_the_max_supply` in `rewards.rs`.

## D-075 Block interval of about 1 second (M10, decided by the project owner; changed back to 5 seconds the same day, keeping the compact headers in payloads and the funding UTXO pool; revises SPEC §7.1)

- **Problem**: the project owner (2026-10-02): "speeding up is also necessary; make it 1 s if possible". The target block interval changes from 5 seconds to 1 second: the DAG parameters must be re-chosen by the block rate,
  parameters counted in blocks must be converted by time; with faster blocks, the data written to BTC (every block's header must land on chain, §7.1, §10.3) and the number of settlement transactions increase accordingly.
- **Decision**:
  1. **Parameters** (devnet and signet):
     - `TARGET_BLOCK_TIME` 1 second; `ASERT_HALF_LIFE` stays 600 seconds (counted in seconds).
     - GHOSTDAG (D-063): `GHOSTDAG_K` 18 (1 block per second, network delay bound 5 seconds, δ = 1 %: P[Poisson(10) > 18] ≈ 0.7 %, the same as Kaspa mainnet),
       `MAX_BLOCK_PARENTS` 10, `MAX_MERGESET_SIZE` 180 (10 k), `MERGE_DEPTH` 3 600 (1 hour, previously 720 × 5 seconds).
     - Parameters counted in blocks keep their original time spans: `SETTLEMENT_DEPTH` 10 (about 10 seconds, previously 2 blocks × 5 seconds), `REORG_WINDOW` 5 000 (about 83 minutes,
       previously 1 000 blocks × 5 seconds). `MEDIAN_TIME_SPAN` stays 11 (it is only used to keep timestamps from going backwards).
     - D-074 item 2: the issuance per second and the halving time are unchanged, `BLOCK_REWARD` 1.98 KSA (198 000 000 base units), `REWARD_HALVING_INTERVAL`
       126 144 000 (1 460 days × 86 400 seconds).
     - After the headers in payloads become smaller (next item), `MAX_SETTLEMENT_HEADERS` rises from 100 to 500 (about 8 minutes of blocks; 500 compact headers are about 25 KB,
       still within `MAX_BLOCK_DATA_BYTES`).
     - The devnet nodes' `--min-block-interval-ms` changes to 1 000.
  2. **Compact headers in payloads** (revises the encoding of SPEC §7.1; the header itself, the block hash and P2P are unchanged): the header sequence in `Settlement` and `Headers` payloads
     is encoded one header at a time relative to the headers before it in the sequence; decoding reconstructs the full headers and computes their hashes:
     - `flags: u8`: bit 0 the version equals the previous one, bit 1 the BTC anchor equals the previous one, bit 2 `bits` equals the previous one, bit 3 `entries_root` is the root of the empty entry list;
       the remaining bits are 0; bits 0–2 of the first header are 0. When equal, the bit must be set (canonical encoding).
     - Version (when different, `u32`); height: the first as a LEB128 varint, afterwards the difference from the previous one as a zigzag varint; parents: a varint count, then each parent as a
       varint `r` -- `r ≥ 1` refers to the hash of the `r`-th header before it in the sequence, `r = 0` is followed by a 32-byte hash (a parent that is in the sequence must use a reference);
       anchor (when different, `u32` height + 32-byte hash); `entries_root` (when not the empty root, 32 bytes); `state_root` 32 bytes; producer: a varint `r`,
       `r ≥ 1` means the producer of the `r`-th header before it (taking the nearest one), `r = 0` is followed by 32 bytes; timestamp: the first as a varint, afterwards the difference as a
       zigzag varint; `bits` (when different, `u32`); `nonce` 8 bytes. Varints must use the shortest encoding.
     - A typical empty block drops from 200–232 bytes to about 47 bytes, a block with entries to about 80 bytes. The encoding of entries (`EntryData`) is unchanged.
  3. **Funding UTXO pool** (local policy of the node): settlement transactions can only be funded by confirmed UTXOs and must not spend each other (§7.2), so within one BTC block interval a producer
     can fund no more settlement transactions than it has confirmed UTXOs. With 1-second blocks and continuous operations this becomes a bottleneck, so when its confirmed UTXOs are fewer than a target count
     (default 64) the node sends a split transaction that splits the largest UTXO into several pieces (not below the amount a settlement transaction needs).
  4. **Settlement batching (pending the project owner's decision; not implemented in this item)**: currently every block containing fast-path entries carries its own settlement transaction. With 1-second blocks and operations in every block:
     each settlement costs about 140 vB of transaction overhead plus about 232 bytes of header and entries, about 600 transactions and about 230 kvB per BTC block interval (about 23 % of BTC block space),
     about 33 000 000 sat / day at 1 sat/vB; each producer also needs about 600 confirmed UTXOs per BTC block interval. With compact headers one settlement is about 230 vB,
     still 5 times that of 5-second blocks. The fundamental fix is one settlement transaction covering multiple blocks (headers and entries in the same payload, the users' path A input/output pairs laid out in entry order;
     `Merged` entries already carry the user-signed inputs themselves, so a later producer can settle on their behalf); combined with the Taproot witness envelope (already mentioned in §7.1, where data costs about
     a quarter of OP_RETURN), at 1 block per second and about 50 bytes per header this is about 7.5 kvB per BTC block interval. This changes block validity (blocks containing fast-path entries
     no longer each carry a settlement transaction), the BTC tracker, the indexer, rescue settlement, header republishing, the backlog limit and the RPC, and requires deciding what a producer that settles for others gets (the path A
     contribution goes to the party that actually settles, the same as rescue settlement), so the project owner is first asked to decide whether and when to do it.
- **Implementation (M10.1)**: the parameters are in DEVNET / SIGNET of `params.rs`; the unit test `one_second_blocks_keep_the_five_second_time_scales` verifies the issuance per second,
  the halving time and the time-converted block counts. The funding UTXO pool is maintained in the node's rescue thread (`maintain_pool` in `rescue.rs`): when the available confirmed UTXOs
  are fewer than `--funding-pool-target` (default 64, 0 disables splitting), the largest one is split into n equal outputs (same funding script, remainder to the first piece; n counts down from the shortfall + 1
  until each piece is at least `FUNDING_SPLIT_MIN_SAT` = 100 000 sat); the input is marked reserved just like for settlement transactions; at most one split transaction is unconfirmed at a time, and it must confirm
  before splitting again. Test `m10_funding_pool.rs`: once the pool is replenished, 12 consecutive blocks within one BTC block interval each carry an operation, and all settlement transactions can be funded and broadcast (without splitting, the 9th
  operation would have to wait for the next BTC block). Time-dependent tests are converted by the parameters (the time scales of the ASERT, convergence and PoW tests; the indexer scenarios use their own test parameters,
  and the reward in `m6_2_mock` is set to 0); `--min-block-interval-ms` for devnet and signet changes to 1 000.
- **Implementation (M10.2)**: `ksana-primitives::compact` (the header-by-header `HeaderEncoder`, `write_headers` / `read_headers`, `encoded_len`),
  `Payload` becomes hand-written Borsh (the variant index and the encoding of entries and VmOps are unchanged; only the header sequence is compacted); the limit of 255 parents is symmetric between encoding and decoding. Measured: with unchanged bits
  an empty block is 46 bytes; ASERT adjusts bits on almost every block (the target changes by about one thousandth per block, and the mantissa of compact bits has 23 bits), in which case it is 50 bytes.
  - Producer policy (local, `payload_headers` in `chain.rs`): the other headers carried are still at most `MAX_SETTLEMENT_HEADERS − 1`, and additionally their compact encoding must not exceed
    half of `MAX_BLOCK_DATA_BYTES` (the rest is left for this block's header and entries); the earliest are dropped when it is exceeded. When many of the headers' parents are outside the sequence (33 bytes each),
    500 headers may not fit, and without this limit the settlement payload could not be assembled.
  - The rescue republishing rule (`rescue.rs`) gains one more case: when, after the first block whose data has not landed on chain, the settlement of a block carrying a settlement transaction is already on chain but did not carry it (the producer
    carried fewer headers because of the size), the headers from it up to the one before that block are republished. The original rule assumed that block carried the nearest `MAX_SETTLEMENT_HEADERS − 1` headers before it;
    when fewer are carried, those headers would never land on chain and the indexer would stall (`m10_payload.rs` fails when this rule is removed). The republish transaction accumulates the compact encoding length header by header.
  - TS SDK: `compact.ts` is byte-for-byte identical to Rust with the same error messages; the type descriptions in `schema.ts` can carry their own codec (`codec`, used only for `Payload`, cannot be nested).
  - Shared vectors regenerated: the `Payload` vectors switch to the compact encoding, with three more groups: parallel blocks with merging, wrapping differences, and maximum values; `invalid` gains 15 groups of compact headers (13 of them each correspond to
    one canonicality rule, with `error`, and Rust and TS reject by the same rule; the others are truncation and trailing bytes). The fuzz target `payload_decode` gains seeds in the parallel-block shape.
  - Tests: `compact.rs` unit tests (varints, the size of a chain of empty blocks, parallel-block round trip, wraparound, non-canonical encodings); `m10_payload.rs` (300 empty blocks land on chain with one settlement,
    each no more than 50 bytes; headers beyond the payload budget are republished after the settlement lands on chain, and the settled prefix advances); TS `compact.test.ts` (including round trips and mutations of pseudo-random sequences).
- **Revision (2026-10-02, decided by the project owner: back to 5-second blocks)**: the project owner: "switch back to 5 s blocks; we cannot afford the fees of 1 s blocks. Compacting empty blocks is necessary. One settlement transaction covering multiple blocks is something we must do."
  - The parameters of item 1 go back to the 5-second-block values (SPEC §13, D-063, D-074): `TARGET_BLOCK_TIME` 5 seconds, `SETTLEMENT_DEPTH` 2, `REORG_WINDOW` 1 000,
    `GHOSTDAG_K` 8, `MAX_BLOCK_PARENTS` 10, `MAX_MERGESET_SIZE` 128, `MERGE_DEPTH` 720, `BLOCK_REWARD` 9.9 KSA (990 000 000),
    `REWARD_HALVING_INTERVAL` 25 228 800; `--min-block-interval-ms` of the devnet and signet nodes goes back to 5 000.
  - `MAX_SETTLEMENT_HEADERS` stays 500: 500 compact headers are about 25 KB, about 42 minutes of blocks with 5-second blocks, so neither a producer going offline briefly nor a stretch of only empty blocks
    needs republishing.
  - Item 2 (compact headers in payloads) and item 3 (funding UTXO pool) are kept. Item 4 (settlement batching) is to be done, see D-077.
  - Implementation: the parameters and the TS SDK's parameters are reverted; the time-dependent tests (ASERT, convergence simulation, single-node block interval) return to their 5-second-block versions, and the tests written in terms of the parameters in M10.1
    (scenarios producing blocks by `SETTLEMENT_DEPTH`, PoW tests) are kept; the unit test `one_second_blocks_keep_the_five_second_time_scales` is removed; the indexer
    scenario tests' parameters `PARAMS` go back to DEVNET. The determinism test's golden roots go back to their pre-M10 values (block timestamps and reward amounts are the same).
- **Impact**: `ksana-primitives` (parameters, compact header codec), `ksana-btc` (payloads), node (parameters, funding UTXO pool), indexer, TS SDK (parameters,
  payload codec), shared vectors, fuzzing seeds, devnet configuration and time-dependent tests. State roots change with the parameters (reward amounts, block hashes).

## D-076 Network upgrade mechanism (M11, the project owner agreed to build an upgrade mechanism; design confirmed 2026-10-05, D-096)

- **Problem**: so far, any consensus change (parameters, encoding, execution rules) could only be made by switching to a new network (a new genesis block). The project owner (2026-10-02) agreed to build
  an upgrade mechanism; SPEC §17 mentions "upgrades are introduced through the block version number". Requirements: a definite activation point, with all nodes and indexers that upgraded their software switching rules at the same block; nodes that have not
  upgraded must stop and alert explicitly rather than silently follow another chain; parameter changes should as far as possible only change data, and rule changes branch by version in the code.
- **Decision**:
  1. **Protocol version = block header version**, activated by DAA score (`|past(B)|` of D-063): each network's parameters carry an upgrade schedule
     `upgrades: &[Upgrade]` (`Upgrade { version, daa_score, rule changes }`, with both `version` and `daa_score` strictly increasing, `version` greater than the base
     version `BLOCK_VERSION` = 2, `daa_score ≥ 1`). The protocol version of block B = the version of the last upgrade whose DAA score does not exceed B's, or 2 when there is none;
     the header's `version` must equal it exactly (`check_header`, D-063): an old version after activation and a new version before activation are both invalid. devnet and signet
     currently have no scheduled upgrades. DAA score is chosen over height or BTC anchor height because it is monotonic along the DAG and proportional to time (about +1 per block interval), and the block reward (D-074) and
     ASERT are already computed from it; the anchor height lags, and parallel blocks can also have different anchors.
  2. **Rules that vary with the version (`Rules`)**: the parameters and the upgrade schedule yield, at a given DAA score, `Rules { version, block_gas_limit, native_max_gas,
     min_vmop_version, max_vmop_version }`; the rules given in an upgrade item override the previous values (`None` means unchanged), and later upgrades keep overriding on top of that. The base values are
     `BLOCK_GAS_LIMIT` and `NATIVE_MAX_GAS` from the parameters and VmOp versions 2–2. An upgrade that only changes these rules needs just one more item in the parameters; other changes (new execution
     rules, encodings, VM features) branch on `rules.version` in the code and activate with the same upgrade item. The payload limits, the block reward and the like are not put into `Rules` for now: the former are already needed at decode
     time (before the block is determined), the latter also requires proving the max supply (D-074); they are added when actually needed.
  3. **Which checks use which block's rules**: the state-independent checks in block validity (header version, total gas of the entries, VmOp versions, etc., `check_entries_static`)
     follow the block's own version; execution (the state transition) follows the main-chain block's rules -- the entries of a merged block execute in the context of the main-chain block that merges it (as with the timestamp, D-073),
     and the whole main-chain block (together with its merge set) uses a single set of rules. Entry-pool admission follows the rules of the next block after the tip.
  4. **Nodes and indexers that have not upgraded**: on receiving a block whose version is higher than the highest version it knows, a node does not judge it invalid and does not penalize the peer; it only refuses it and alerts "node software
     needs to be upgraded" (once per version), so that after upgrading the software and restarting it syncs that chain as usual. Other version mismatches are still judged invalid. The indexer likewise stops before the fork and
     alerts.
  5. **Observability**: the node gains `ksana_getChainInfo` (tip, DAA score, protocol version, the next scheduled upgrade and the highest version this software supports, `docs/RPC.md`);
     the node and the indexer print the upgrade schedule at startup. The TS SDK's parameters carry the same schedule plus `rulesAt`, and the RPC client has `getChainInfo`.
  6. **State migration at activation**: no upgrade in this round needs a migration, so no migration hook is made; when needed it runs before the first main-chain block of that version executes and is defined with that upgrade.
  7. **Upgrade procedure** (recorded here for later upgrades to follow): implement the rule changes in the code under the new version number (the behavior of old versions stays unchanged, and the determinism test's golden roots
     stay unchanged); add an item to each network's upgrade schedule (the DAA score leaves at least enough time for release and deployment); sync the shared vectors and the TS SDK; run a node and indexer test crossing the activation point
     on regtest / devnet with test parameters; release the software, and all nodes and indexers complete the upgrade before activation.
- **Rationale**: the header version is already in every header and protected by PoW, so identifying the rule set with it needs no new field; activating by DAA score is consistent with the existing deterministic computations;
  refusing unknown versions without penalizing avoids nodes that have not upgraded banning upgraded peers and then having to re-establish connections after upgrading.
- **Implementation (M11)**:
  - `Params::rules(daa_score)` / `protocol_version` / `next_upgrade` / `max_known_version` / `check_upgrades` (checked when the node and the indexer open;
    an invalid schedule refuses to start). `check_header` first checks whether the version is higher than the highest this software knows (`ConsensusError::UnknownVersion`, which the node turns into
    `NodeError::Rejected` and the network layer treats as Ignore), then whether it equals the version at that DAA score (`BadVersion`, invalid block).
  - Execution: `BlockEnv.rules` is the main-chain block's rules; `check_entries_static(params, rules, entries)` uses the block's own rules (the node and the indexer take the DAA score from each block's
    GHOSTDAG data). Entry-pool admission follows the rules of the next block after the tip (`Mempool::set_rules`, updated on admission and when the main chain changes); when producing a block, the header
    version is the version at the template's DAA score.
  - Item 5 originally had the indexer's `indexer_status` give the protocol version too: the status the indexer publishes does not retain the DAA score across rollback and re-verification, and changing the publishing path for this is not worth it,
    so instead it only prints the upgrade schedule at startup and alerts when it meets an unknown version (once per version).
  - Tests: `rules_follow_the_upgrade_schedule` and `bad_upgrade_schedules_are_rejected` in `ksana-primitives`; `header_version_follows_the_upgrade_schedule` in
    `ksana-consensus`; `ksana-execution/tests/upgrade.rs` (gas limit, native op limit and VmOp versions by the block's
    DAA score); `ksana-node/tests/m11_upgrade.rs` (a single node crossing the activation point, and the RPC; a node that has not upgraded stops before the fork while staying connected to its peers, and syncs after restarting
    with the new parameters); `ksana-indexer/tests/upgrade.rs` (block-by-block agreement under the same upgrade schedule; an indexer that has not upgraded stops before the activation point); TS tests for `rulesAt` and
    `getChainInfo`.
- **Impact**: `ksana-primitives` (`Upgrade`, `Rules`, the upgrade schedule in the parameters and its checks), `ksana-consensus` (header version check), `ksana-execution`
  (`BlockEnv.rules`, static checks and execution of entries by rules), entry pool, node (header version when producing, admission, `ksana_getChainInfo`, handling of unknown versions), indexer,
  TS SDK (parameters, RPC client). With an empty upgrade schedule the behavior is exactly as before (state roots unchanged).

## D-077 Settlement batching: one settlement transaction carries the data of multiple blocks (M12, decided by the project owner; design confirmed 2026-10-05, D-096; revises SPEC §6.3, §7.1, §7.2 and D-034, D-038, D-043, D-046, D-047, D-063)

- **Problem**: every block containing non-forced entries carries its own settlement transaction (D-043). With 5-second blocks and operations in every block, that is about 120 settlement transactions per BTC block interval,
  each with about 140 vB of fixed overhead (funding input, change, P2A and the fixed part of the data output) and each needing a confirmed funding UTXO; the size of the data itself (headers and entries)
  does not depend on the number of transactions. The project owner (2026-10-02): "one settlement transaction covering multiple blocks is something we must do".
- **Decision**:
  1. **Which blocks carry a settlement transaction** (revises D-043):
     - A block containing path A (`Merged`) entries must carry a settlement transaction: the user-signed input/output pairs can only be spent by it or by its rescue settlement (D-034, D-045 unchanged).
     - Other blocks may carry one or not. Empty blocks and blocks containing only forced-inclusion entries may also carry one, to carry other blocks' data.
     - For a block that contains non-forced entries (path B, fee debt, unlock) but carries no settlement transaction, its block body (all of its entries in payload form, the same as the entries of a settlement payload) is carried on chain by
       any later KSN1 payload: usually the settlement transaction of some later block, but it can also be a republish transaction sent by anyone.
  2. **Payloads** (revises §7.1; the compact header encoding and the entry encoding are unchanged): `Settlement { headers, entries, bodies }`, `Headers { headers, bodies }`.
     `bodies: Vec<Body>`, `Body { header: u32, entries: Vec<EntryData> }`: `header` is the index of a header in the same payload, `entries` are all the entries of that block.
     Canonicality (checked at decode time; decoding fails when it is not satisfied):
     - `header` strictly increasing and less than the number of headers; in `Settlement` it must not point at the last header (this block's entries are in `entries`).
     - A body contains no `Merged` entries (a path A block is carried only by its own settlement transaction) and at least one entry that is not `Forced` (a block containing only forced-inclusion entries
       needs no body).
     Encoding: the variant index, the compact header sequence, for `Settlement` then this block's entries (Borsh), and finally `bodies` (Borsh: a `u32` count, each item a `u32` index and
     an entry list). The whole payload still must not exceed `MAX_BLOCK_DATA_BYTES`, and there are still at most `MAX_SETTLEMENT_HEADERS` headers.
  3. **Checking the settlement transaction a block carries** (revises the payload part of D-034; the transaction structure, signatures and funding confirmation are unchanged, `n` is the number of `Merged` entries in this block): the last header
     is this block and `entries` are all of this block's entries (unchanged); every item in `bodies` must point at the header of a block in this block's past, and its `entries` must equal all the entries of that
     block. The remaining headers may still be arbitrary (self-certified by PoW, D-063).
  4. **Data landing height** (revises D-038, D-063):
     - A block containing path A entries: the earliest confirmation height of its valid settlement (a confirmed settlement per D-034, including rescue settlements) (unchanged).
     - Other blocks containing non-forced entries: the earliest confirmation height of a KSN1 payload that carries its header and body (the last header and `entries` of a settlement payload, or an item in the `bodies` of a settlement / republish
       payload). That transaction itself need not be a valid settlement: the body is self-certified by `entries_root`, just as the header is self-certified by PoW.
     - Empty blocks and blocks containing only forced-inclusion entries: the height at which the header first appears (unchanged).
     - The settled closure, unlocking, BTC finality and the indexer all follow this definition. When an `Unlock`'s `settlement_txid` is not all zeros, it must be one of the transactions above that landed that block's data
       on chain (since D-074 producers write only all zeros).
     "Whether data must land on chain" and "under which rule" depend only on the block's content (whether it has non-forced entries, whether it has `Merged` entries), so the indexer gets the same result as the node from BTC data
     alone; whether the block carries a settlement transaction does not affect its data landing height.
  5. **Settlement backlog limit** (revises D-046): B is a block in N's past that contains non-forced entries and whose data has still not landed on chain (per item 4) as of N's anchor height; the condition is unchanged:
     when `N.anchor.height ≥ B.anchor.height + SETTLEMENT_TIMEOUT`, N may contain only forced-inclusion entries. N may still carry a settlement transaction that carries the backlogged bodies, and thereby
     clear the backlog.
  6. **Producer policy** (local):
     - Carried data: the header selection of D-063 is kept (starting from the parents, look at the blocks in the past in descending order of blue work, and on meeting a block that carries a settlement transaction do not search further into its past),
       and in addition collect the bodies (together with the headers) of the blocks among them that carry no settlement transaction, need a body and whose data has not yet landed on chain. Compact headers take at most half of the payload limit; the bodies and
       this block's entries share the rest, and the earliest are dropped when they do not fit (republishing fills them in later).
     - When to carry a settlement transaction: it must when this block includes path A entries. Otherwise it carries one when there are bodies to carry (this block's or backlogged ones) and no settlement transaction is in flight:
       "in flight" means that among the nearest blocks in this block's past that carry a settlement transaction (those the header selection meets) there is one whose headers have not yet landed on chain and whose anchor is at most 2 blocks behind the BTC tip
       (one stuck in the mempool longer is no longer waited for; rescue and the backlog limit handle it). It also carries one when the backlogged bodies in total, or the body of this block's own entries, exceed one eighth of the payload limit,
       without waiting for the in-flight settlement transaction: carried bodies take at most a quarter, a block only becomes backlogged when it is under one eighth, so before the trigger the backlog is at most a quarter,
       fits in one payload and does not squeeze out the earliest bodies (judged by the backlog before trimming).
       Thus with little data the block of each operation settles immediately (as before); with continuous operations, the bodies that back up while one settlement transaction is in flight (about one BTC block)
       are carried all at once by the next block after it confirms, so each BTC block interval usually has just one batch settlement transaction, plus the settlement transactions of the blocks containing path A entries (which
       also carry the backlogged data). (The design originally said "carry one when the backlogged bodies come from an earlier BTC block"; the implementation judges by in-flight transactions instead: with little data there is no need to wait for a
       BTC block, and the effect is the same.)
     - Local switch `--no-settlement-batching` (`Funding::with_batching(false)`): every block that needs a body carries its own settlement transaction (the pre-D-077
       behavior: faster BTC finality, more settlement transactions).
     - When funding is temporarily short, no settlement transaction is carried: path B, fee debt and unlock entries are included as usual (the body is carried by a later settlement), path A entries are not included (as now).
       A producer without a configured funding wallet still produces only empty blocks and blocks containing only forced-inclusion entries (it generates no bodies that others would have to pay to put on chain).
  7. **Broadcast, rescue and republishing** (revises D-037, D-045, D-047):
     - Broadcast timing is unchanged: when the main-chain block that accepts the block has `SETTLEMENT_DEPTH` confirmations.
     - Rescue: when the settlement transaction a block carries cannot be broadcast and the data it carries has not all landed on chain, it is rebuilt in the order of D-045, D-057 (payload unchanged). "The data it carries" is
       this block's data (for a block containing path A entries, its valid settlement), the other headers and the data of each block in `bodies`. The settlement record is closed when its transaction confirms or when all the data it carries
       has landed on chain.
     - Republishing: when the first block in main-chain acceptance order whose data has not landed on chain carries no settlement transaction, the rules are the same as D-047, D-075; for blocks that need a body, the republish payload also republishes the body
       (`Headers { headers, bodies }`). When judging "no later settlement payload will carry it", only the blocks in the past that contain it and carry a settlement transaction are considered: a block that comes after it in acceptance order
       yet is parallel to it (a merged block in the DAG) will not carry it, and that block landing on chain first does not mean this block was missed. (Found in a local 3-node soak run: batching makes the block after an empty block
       carry no settlement transaction, a side-chain block parallel to the empty block that carries a settlement transaction landed on chain first, and each of the three nodes republished the empty block's header once.)
  8. **RPC**: `ksana_getSettlement` also returns a result for blocks that carry no settlement transaction but need their body on chain: `carrier` is the block whose settlement payload carries its body,
     `txid` / `txid_rpc` / `tx` are the settlement transaction carried by that block (`null` while no block carries it yet), `status` and `confirmed` are the on-chain status of its body
     (`confirmed` gives the transaction that landed it on chain). For blocks carrying a settlement transaction `carrier` is `null`, and the rest is as before. Empty blocks and blocks containing only forced-inclusion entries that carry no settlement
     transaction still return `-32004`.
- **Implementation (M12)**:
  - `ksana-primitives`: `Body`, `check_bodies`; the hand-written Borsh of `Payload::Settlement { headers, entries, bodies }` and `Payload::Headers { headers, bodies }`
    checks canonicality at decode time (same error messages as the TS SDK).
  - `ksana-btc::payload`: `needs_body` (formerly `needs_settlement`: the body must land on chain), `requires_settlement_tx` (contains path A entries), `bodies_in`
    (the bodies a payload carries: the settlement payload's own block and the blocks in `bodies`), `body_len`; `payload` takes `bodies`; `settlement_belongs_to` returns the other bodies the payload
    carries. Tracker (`tracker.rs`): `KnownBlocks::block_data` answers `Unknown` / `NoBody` / `Body` (replacing `settlement_context`);
    while scanning, for every body a payload carries it determines whether it can land that block's data on chain (blocks containing path A entries still go through `validate_confirmed`) and records it in the same table, so the code for data landing
    heights, the settled closure and the backlog limit is unchanged; `BlockSummary.settles` now means "the body must land on chain".
  - Node: block validity (`check_body`: only blocks containing path A entries must carry a settlement transaction; `check_carried_bodies`: bodies belong to blocks in this block's past and match their
    entries); the DAG's block info separates `settles` (the body must land on chain) from `carries` (carries a settlement transaction); the producer's `payload_carry` (headers and bodies, the in-flight
    judgment, dropping the earliest by budget) and the decision whether to carry a settlement transaction (`Carry::settlement_due`, `PAYLOAD_BODY_TRIGGER`); `carried_on_chain` (the condition for closing settlement records and for rescue);
    republishing (`republish`) also republishes the body for blocks that need one; `body_carrier` and the `carrier` of `ksana_getSettlement`. A producer without a funding wallet
    still produces only empty blocks and blocks containing only forced-inclusion entries.
  - Indexer: candidate bodies are also taken from `bodies` (`known.rs`); `block_data` is the same as the node's.
  - TS SDK: `Body`, `checkBodies`, the payload codec takes `bodies`; `txid` / `txid_rpc` / `tx` of `SettlementInfo` may be `null`, and `carrier` is added.
  - Shared vectors: two groups of settlement and republish payloads with bodies, 6 groups of non-canonical bodies (with `error`); the fuzz target `payload_decode` gains a seed for a republish payload with bodies.
  - Tests: `payload_bodies_must_be_canonical` in `ksana-primitives` and the payload property tests (the strategy generates canonical bodies);
    `ksana-node/tests/m12_batch.rs` (backlog while in flight, after confirmation one batch settlement transaction carries the bodies of three blocks and they enter the settled closure, with a cost comparison against per-block settlement;
    a path A block settles immediately and carries the backlogged bodies; when the backlog during flight exceeds the threshold the next block carries a settlement transaction and no backlogged body is left out; with batching off
    blocks settle one by one; a stuck settlement transaction does not block the next batch; a block with a tampered body and a path A block with its settlement transaction removed are invalid; no republishing when a side-chain block parallel to an empty block that carries a settlement transaction
    lands on chain first); `bodies_carried_by_a_batch_settlement_are_indexed` in `ksana-indexer/tests/indexer.rs`; regtest `m12_regtest.rs`
    (bitcoind accepts and confirms the batch settlement transaction, the payloads carry bodies by height, unlocking proceeds as usual afterwards, and the indexer's and the node's state roots agree block by block). The scenario tests written for per-block settlement
    (the node's and indexer's `Side`, `m6_1_anchor`, `m10_funding_pool`, regtest `m3_regtest`) turn batching off. Cost: in the mock, the fixed overhead of each settlement transaction is about
    144 vB; 4 blocks with operations use 2 settlement transactions where per-block settlement needs 4 (2 189 vB versus about 2 477 vB in one run). With 5-second blocks and operations in every block, each BTC block
    interval goes from about 120 transactions to about 1, saving about 119 × 144 ≈ 17 kvB.
- **Rationale**: the fixed transaction overhead and the funding UTXOs are counted per batch rather than per block, and the amount of data is unchanged (headers are already compact, D-075). Path A blocks still settle immediately: the contribution is their
  settlement fee, and delaying settlement would only lengthen the user's double-spend window (D-044, D-058). A body is self-certified by `entries_root`, so a payload sent by anyone can land it on chain, and when the producer is offline
  other nodes can still carry it on chain, with no new trust assumption.
- **Costs and remaining issues**:
  - A path B operation in a block without a settlement transaction becomes BTC-final only with the next batch (usually after the next BTC block), about one BTC block later than now; unlocking is delayed likewise.
  - A producer that carries bodies for others pays the BTC fee with no direct income; its own income can only be unlocked once the data of all blocks in its past has landed on chain, so it has an incentive to do so.
    A producer that can produce blocks but does not fund can free-ride; when nobody carries, the backlog limit pauses the fast path. Paying carriers a fee is left for later.
  - Producers of parallel blocks may carry the same batch of bodies at the same time and pay the fee of one extra transaction (the same as parallel blocks each carrying headers now).
- **Impact**: `ksana-primitives` (payload encoding and canonicality checks), `ksana-btc` (payload construction and verification, the tracker's data landing heights and backlog limit), node (block validity,
  producer policy, settlement records, rescue and republishing, RPC), indexer (body sources), TS SDK (payload codec, RPC types), shared vectors, fuzzing seeds and documentation.
  The structure of settlement transactions (the inputs and outputs of D-034) and the execution layer are unchanged; execution results and state roots are unchanged.

## D-078 Abandon path A: paying with BTC becomes "top-up for KSA + path B" (M13, decided by the project owner; revises SPEC §4.1, §5.1, §6.1, §7.2, §8 and D-002, D-033, D-034, D-044, D-045, D-054, D-058, D-074, D-077)

- **Problem**: path A merges the user's input/output pair, signed with `SIGHASH_SINGLE|ANYONECANPAY`, into the producer's settlement transaction, which has several structural flaws:
  - The signed pair has to be published over P2P, and anyone can put it into their own transaction and send the contribution to themselves; on-chain there is no telling a third party from the user themself (the D-044 exemption and fee evasion).
    Once path A has volume, bots will intercept contributions in bulk: intercepted before inclusion, the user pays for nothing and the op cannot execute; intercepted after inclusion, the producer works for nothing.
  - Each op needs a confirmed UTXO that is not in use (a UTXO can be used only about once per BTC block), and wallets have to support a special sighash; a block containing path A must be settled immediately and
    cannot be batched; each op takes about 100 vB more on BTC.
  The project owner (2026-10-02): "just drop path A; use top-up + path B instead".
- **Decision**:
  1. **Delete path A and fee debt**: `FeeMode::BtcMerged`, `Entry::Merged`, `EntryData::Merged`, `Contribution`, state `0x07` (funding records). Fee debt (§8.4)
     existed only for path A and is deleted along with it: `Entry::Debt`, `AccountState.fee_debt`, "only path A is allowed while in fee debt", the D-058 local list and the tracker's watch on funding
     inputs (D-044). The settlement transaction no longer has user input/output pairs (n = 0 in D-034), and rescue only handles conflicts on the producer's inputs (D-045); "a block containing path A
     entries must carry a settlement transaction" in D-077 disappears with it, and any block's body lands on BTC carried by a payload (the only rule left for the data-landing height is "a payload that carried the same body").
     The unlock entry drops `settlement_txid` (written as all zeros since D-074).
  2. **Encoding**: the Borsh variant indices of the enums and structs above shift forward accordingly (`FeeMode`: `Token` = 0, `BtcNative` = 1; `Entry` / `EntryData`: `Forced` = 0,
     `Sponsored` = 1, `Unlock` = 2), which is a consensus encoding change. Neither devnet nor signet carries any value yet, so both simply restart rather than going through the D-076 upgrade schedule; the shared
     vectors and the golden roots of the determinism test are regenerated.
  3. **How to pay with BTC: top-up**. The user sends an ordinary BTC transaction through path C (any wallet, `SIGHASH_ALL`) that has both an output paying the seller and a KSN1 data output carrying
     a "claim" op; once BTC confirms, this op is force-included, and the exchange contract verifies the payment and releases the KSA. Subsequent ops go through path B, take seconds and can be batched.
     - New host function `native_tx_outputs`: a path C op reads all outputs (amount and script, in transaction order) of the BTC transaction that carries it; ops on other
       paths get an error code when they call it. The transaction comes from the anchored view (`BtcView::transaction`), the result depends only on the anchor chain, and gas is charged per byte.
     - Exchange contract (example `contracts/examples/ksa-exchange`): the seller puts up KSA as collateral and lists a sell order (payment script, price: how many KSA base units per sat);
       the buyer calls `claim(order)` in the payment transaction, and the contract converts the amount paid to that order's payment script into KSA at the price and releases it to the buyer, up to the order's balance; the part that does not fit is recorded as
       a debt from the seller to the buyer, and when the seller later adds collateral, the debts are made good first, in order. A wallet should verify the order's balance before paying, and a seller should use a new payment address for each order.
     - The claim op goes through path C: no Ksana fee, only the BTC miner fee, with gas limit `NATIVE_MAX_GAS`.
  4. **The producer's BTC income**: there are no more contributions. The producer receives sponsored fees and block rewards in KSA, lists sell orders on the exchange contract to obtain BTC, and uses it to pay the fees of settlement transactions
     (part of §17 "the producer's BTC advance threshold").
  5. **Local policy and interfaces**: the entry pool, block production and op admission drop path A; `ksana_submitOp` drops the PSBT parameter; `ksana_getAccount` drops `fee_debt` and
     `path_a_refused`; `ksana_estimateFee` drops `path_a` and adds a suggested fee for top-up transactions; the P2P op message no longer carries a `Contribution`;
     `ksana-cli` and the TS SDK drop path A and add the top-up flow (the SDK builds the path C transaction with payment + claim and hands it to the wallet for signing).
- **Cost comparison** (estimated): path A is about 470 vB per op (an entry of about 368 bytes + an input/output pair of about 100 vB), and a block containing it has to be settled separately; path B is 217 vB per op
  and can be batched; one top-up is about 400 vB (paid by the user to BTC miners), about 7 vB per op when amortized over 60 ops. The total is about 225 vB per op, about half of path A.
- **Cost**: the first top-up has to wait for one BTC confirmation (about 10 minutes); after that it runs on the balance and automatic top-ups; users who do not want to hold KSA can only use path C (about 10 minutes per op).
  When an order's balance in the exchange contract is insufficient there is a debt record rather than a refund (a contract cannot issue refunds on BTC mainnet).
- **Impact**: almost every crate (types and encoding, execution, BTC transaction assembly and verification, tracker, entry pool, node, RPC, P2P, CLI, indexer), the TS SDK, the example contracts,
  the shared vectors, the fuzzing seeds and all related tests; this entry takes precedence over the SPEC's provisions on path A and fee debt.
- **Implementation** (M13.1, the deletion part; top-up is in M13.2):
  - Types and encoding: `FeeMode { Token, BtcNative }`, `Entry` / `EntryData { Forced, Sponsored, Unlock { block_hash } }`, `AccountState { nonce }`;
    `Contribution` is deleted; `OutPoint` (D-001) no longer appears in consensus types, but its definition and its shared vectors are kept (Rust and TS SDK). The shared vectors and the golden roots
    of the determinism test are regenerated.
  - Execution layer: `BtcView` retains only `header(height)` and `block_settled(block_hash)` (which unlock entries are judged by); `transaction` and the queries for funding outputs and conflicting transactions
    are deleted along with path A and fee debt. `MergedBlock` no longer carries "skipped input pairs".
  - `ksana-btc`: `contribution.rs` (PSBT and funding inputs) and `fee.rs` (suggested path A contribution) are deleted; input signature verification moves to `spend.rs` (P2TR
    key-path, P2WPKH, BIP86 tweak). Settlement transaction: the inputs are all the producer's P2TR (BIP86) or P2WPKH, and the outputs are, in order, change, KSN1 data, P2A
    (240 sats); `without_pairs` is deleted. The tracker no longer watches funding inputs, and data landing is determined only by "a payload that carried the same body" (`validate_confirmed` deleted).
    The inherent method `confirmed_output` of `AnchoredView` lets settlement transaction verification fetch the previous outputs of the producer's inputs.
  - Node: the entry pool, block production, op admission and rescue only handle path B and the producer's own inputs (rescue = after the producer's inputs of a settlement transaction are spent, rebuild it with new funding,
    payload unchanged); the D-058 refusal list and `--min-gas-price-msat` are deleted.
  - Interfaces: `ksana_submitOp(op)` has a single parameter; `ksana_getAccount` is `{ account, nonce, balances, locked_fees, at }`; `ksana_estimateFee`
    takes no parameters and returns `{ feerate_sat_vb, path_b: { amount }, at }` (top-up transactions refer to `feerate_sat_vb`). The P2P op message is `OpMessage { op }`;
    the gossip topics and the sync protocol name are unchanged (devnet restarts from scratch, so old and new nodes never run mixed). `ksana-cli` drops `--funding` / `--funding-key`. The TS SDK deletes
    `pathA.ts`, the path A parts of `btc/psbt.ts` and the wallet's path A flow, and parsing of RPC results rejects unknown fields (when the interface gains fields, the SDK has to be updated in step).
  - Tests: the path A and fee debt tests are deleted; the scenarios that still make sense switch to path B -- M3 acceptance criteria 1 and 2 (the settlement transaction has only the producer's inputs; the ops of 20 users share one
    settlement), M4 acceptance criterion 3 (both sides of a fork include the same op; the lighter side's settlement transaction still lands on BTC after the merge, and no two transactions spend the same output), M5 acceptance criteria 2 and 3
    (after the producer's funding input is spent, this node or another node rescues and rebuilds it), the M6 SDK end-to-end tests and demo page (path B and path C); M3 acceptance criterion 3 (the input/output
    pair is removed) and M6 acceptance criterion 2 (path A on signet; `docs/SIGNET.md` changes to manual steps for path B) are void.
- **Implementation** (M13.2, top-up):
  - The host function is renamed `native_tx` and returns the transaction ID and all outputs (Borsh `NativeTx { txid, outputs: Vec<NativeOutput { value, script }> }`,
    2,000 + 1/B gas): a contract needs the transaction ID to keep the same payment from being credited twice within one op (one op can call the same method several times via other contracts).
    Path B ops and read-only calls get `-1`. The import whitelist and the gas table grow accordingly (consensus parameters).
  - Execution layer: `BtcView::native_outputs(txid)`; when a forced-inclusion entry executes its action, `btc_txid` is handed to the host, and the view is queried only once per op.
    `AnchoredView` first looks in the snapshot for native transactions within the anchor height (`NativeTx` gains `outputs`); outside the snapshot it queries bitcoind by transaction ID and checks that
    the confirming block is on the anchor chain with a height not above the anchor height; when the anchor is invalid, it follows the current best chain (D-056). Not finding it is a local fault (these transactions were already checked by the forced-inclusion rule).
  - **The claim op is signed with a one-time key (nonce 0), and the KSA is credited to the beneficiary in the arguments** (`claim(order, beneficiary)`, a refinement of design item 3):
    a path C nonce must equal the account's current nonce or the op is void (D-018), while the BTC has already been paid. If the buyer's account signed it, the claim would be void whenever the buyer
    also sent a path B op before the BTC confirmation, or several top-ups confirmed in an order different from their nonce order. A one-time account's nonce is always 0, so neither case can happen;
    the wallet also only needs to sign this BTC transaction, with no BIP322 message signature.
  - Exchange contract (`contracts/examples/ksa-exchange`): `list` (payment script 1–64 bytes, not `OP_RETURN`, unique within the contract; price > 0;
    the attached KSA becomes the balance), `deposit` (repays debts first, in order, at most 32 per call, and reverts when something is left over after repaying while debts are still outstanding; the remainder goes into the balance and is refunded to the seller when the order
    is closed), `withdraw`, `close` (takes back the balance; cannot be reopened), `claim` (path C only; the same transaction claims against the same order only once;
    `sats × price` uses saturating multiplication, so a claim never fails on overflow), and the read-only `order` / `order_of_script` / `debts` / `claimed` / `quote`.
    The invariant "no unpaid debts while the balance is greater than 0" is maintained, so a new claim never gets ahead of the debts queued before it.
  - TS SDK: `buildNativeTx` gains `payments` (after the data output, before the change); `topUp.ts`: `decodeOrder`, `getOrder`, `claimArgs`,
    `quoteTopUp`, `buildTopUp` (refuses when the order is closed or its balance is insufficient; with `allowDebt` it builds anyway). CLI: `ksana-cli top-up` (funded by the bitcoind
    wallet; `--allow-debt`).
  - Measured (`m13_regtest`): a top-up transaction is 381 vB (P2TR funding, payment, change and the data output carrying the claim op), and the claim entry is 33 bytes in the settlement payload;
    a path B entry is 217 bytes each. For 60 KSA transfers: top-up + path B total 13 434 vB, the old path A about 28 140 vB (about 469 vB each), the former being
    47% of the latter; path B ops after the funds arrive take effect in about 0.4–0.8 seconds.

## D-079 Empty blocks do not change the state root (M14.1, decided by the project owner; revises D-017, D-074, D-075)

- **Problem**: every block writes state: the block reward is recorded as a pending record, moved to `0x08 ‖ block hash` at the start of the next block, queued for unlock, and the KSA total supply increases (D-017, D-074).
  An empty block's state root therefore changes too, and the 32-byte state root in the compact header (D-075) cannot be saved; 32 of an empty block header's 46 bytes are the state root, and with 5-second blocks the roughly 120 headers
  per BTC block interval all have to land on BTC. The project owner (2026-10-02) asked for this to be done together.
- **Decision**:
  1. **Sponsored fee records are keyed by acceptance position**: `0x08 ‖ main-chain height (u64 big-endian) ‖ acceptance index (u32 big-endian)`, where the acceptance index is the block's position in the acceptance order of the main-chain block that accepts it
     (merged blocks are 0, 1, ... in turn, and the main-chain block itself comes last). Written only when the block collected sponsored fees; the position is known at execution time, so "pending record, moved at the next
     block" is no longer needed (D-017 canceled). The pending-unlock queue (`0x09` / `0x0A`) holds only positions that have a sponsored fee record.
  2. **Block rewards are paid out at unlock**: when executing an unlock entry, using the unlockable segment given in `BlockEnv` by the node / indexer (every block in acceptance order after the last unlocked position up to the target block:
     position, hash, producer, DAA score, whether it earns a reward -- main-chain blocks and blue merged blocks), the reward is credited directly to the producer's balance and the KSA total supply increases (still capped at
     `KSA_MAX_SUPPLY`); the sponsored fee records in the segment are released at the same time. A segment has at most `MAX_UNLOCK_BATCH` blocks, and the state records the position unlocked up to.
  3. The **unlock entry** becomes `Unlock { height, index }` (the target block's acceptance position); the target block must already be in the settled closure (`BtcView::block_settled`, with the hash taken from
     the segment in `BlockEnv`).
  4. The **compact header** gains a flag bit: the state root is not written when it equals that of the previous header in the sequence (an extension of the D-075 encoding).
  5. Result: when neither the main-chain block nor the blocks it merges have any entries, no state is written at all, the state root equals the selected parent's, and an empty block header goes from 46 to 14 bytes (estimated).
- **Cost**: the execution layer needs the node / indexer to supply the unlockable segment (the same trust model as the merged-block information: both sides compute it deterministically from the DAG); block rewards go from "locked
  when the block executes" to "paid out at unlock", and the locked balance in the RPC is instead computed by the node from the DAG for display.
- **Impact**: `ksana-execution` (reward and sponsored fee accounting, unlock), `ksana-primitives` (unlock entry, compact header), node and indexer (unlockable segment, unlock
  entries, RPC), the TS SDK, the shared vectors, the golden roots of the determinism test.
- **Implementation** (M14.1):
  - State: `0x08 ‖ main-chain height u64 big-endian ‖ acceptance index u32 big-endian` → `LockedFee { producer, amount }`, written only when that block collected path B sponsored fees
    (a merged block's entries are recorded at its own position and belong to its producer); `0x09` → `UnlockCursor { height, index }` (the position unlocked up to; when absent, nothing has been
    unlocked yet). The unlockable segment already lists the positions one by one and the sponsored fee records are looked up directly by position, so **the pending-unlock queue is no longer needed** (the queue in design item 1 is canceled, `0x0A` is no longer
    used, and D-017's pending record and "moved at the next block" are canceled with it).
  - `BlockEnv`: drops `parent_hash` and `daa_score` (the rules are still chosen by DAA score, in `rules`) and gains `unlockable: Vec<UnlockItem>` (the positions on the selected parent's
    main chain after the cursor of the parent state in acceptance order, at most `MAX_UNLOCK_BATCH`: height, acceptance index, block hash, producer, DAA score, whether it earns a
    reward). `MergedBlock` drops `blue` and `daa_score`. The node and the indexer use the same function `ksana_btc::tracker::unlock_segment` to compute it from the main-chain summary
    (`BlockSummary` / `MainEntry` gain producer, DAA score and whether it earns a reward: true for the main-chain block itself and for blue merged blocks); the node's `Chain::block_env`
    requires the block's selected parent to be the current main-chain tip (so the main-chain summary agrees with the parent state), otherwise it returns `TipChanged`.
  - Unlock entry `Unlock { height: u64, index: u32 }`: first the checks that depend only on the state and the segment -- the segment has at most `MAX_UNLOCK_BATCH` items and is strictly increasing in acceptance order (otherwise
    it is a node / indexer fault, `Fatal`), the target is after the cursor (otherwise invalid: "already unlocked") and in the segment (otherwise invalid: nonexistent or over the limit) -- then the query to the
    BTC view: the target's block must already be in the settled closure. Then, for each position after the cursor up to the target: a block that earns a reward is paid the block reward for its DAA score (total supply capped at
    `KSA_MAX_SUPPLY`, first come first served in acceptance order, the KSA total supply increases), and then the sponsored fee record at that position is released (a record whose producer disagrees with the segment is `Fatal`), both credited to
    the producer's available balance; the cursor is written as the target. A merged block's unlock entry is judged in lenient mode against the same segment of the main-chain block and the current cursor (which earlier merged blocks may
    already have advanced).
  - Producer (`Chain::unlock_entries`): the last position in the segment whose acceptance height does not exceed the settled height at the anchor (`settled_height`)
    (`tracker::unlock_target`); one BTC block interval is about 120 positions, far below `MAX_UNLOCK_BATCH` 1 024.
  - Flag bit 4 of the compact header (`SAME_STATE_ROOT`): set when the state root equals that of the previous header in the sequence, and the 32 bytes are then not written; the first header cannot have it set, and when they are equal it
    must be set. After the first, each header in a run of empty blocks is 14 bytes (flags 1, height 1, parent 2, producer 1, timestamp 1, `nonce` 8); when ASERT adjusted `bits`,
    4 more bytes are added.
  - RPC: `locked_fees` in `ksana_getAccount` is computed by the node: for each block after the cursor produced by that account, the amount is the sponsored fee record plus the block reward that will be paid out at unlock
    (simulating the cap in acceptance order); the total supply in `ksana_getAsset` includes only rewards already paid out.
  - The state store gives the same root for versions without changes (the `versions_without_changes_keep_the_root` test in `ksana-state`), and the node and indexer still commit an empty write set
    for every height.

## D-080 Entry compression in payloads (M14.2, decided by the project owner; revises D-003, D-075)

- **Problem**: entries land on BTC as raw Borsh: a path B transfer is 217 bytes, of which the 32-byte public key, the 16-byte amount, the 4-byte `chain_id` and so on are all uncompressed.
- **Decision**: change only the encoding in the payload (the same idea as the D-075 header compression); the Borsh encoding, signatures and `entries_root` of `Entry` / `VmOp` are unchanged, and decoding restores them byte for
  byte:
  - `chain_id` does not go on-chain (it is filled back in from the network parameters); `nonce`, `max_gas`, and all amounts and lengths use LEB128 varints (the shortest encoding is required).
  - 32-byte keys (account, contract, asset, recipient) use a dictionary within the payload: written in full on first occurrence and as an index afterwards; the dictionary is pre-seeded with the KSA asset ID.
  - The acceptance position of an unlock entry uses varints (D-079); a forced-inclusion entry still writes only the BTC transaction ID.
  - Canonicality is the same as in D-075: a non-shortest encoding, an out-of-range index, or a key written in full where it should have been a reference all make decoding fail; Rust and the TS SDK agree byte for byte on the shared vectors.
- **Effect** (estimated): a path B transfer goes from 217 to about 145 bytes when the account appears for the first time and about 85 bytes when it recurs. The signature (64 bytes) is not compressed; a global
  dictionary indexed by account number and signature half-aggregation are left for later (the former makes decoding depend on state, the latter is not a standard Bitcoin scheme).
- **Impact**: `ksana-primitives` (payload encoding and decoding), `ksana-btc` (payload length estimation), node and indexer (payload construction and parsing, with unchanged interfaces), the TS SDK,
  the shared vectors, the fuzzing seeds.
- **Implementation** (M14.2):
  - The format is in the module documentation of `crates/ksana-primitives/src/compact_entries.rs` (the TS SDK's `compactEntries.ts` is byte-identical). An entry is a `u8` variant plus
    fields; a VmOp is, in order, `version`, key `account`, varint `nonce`, fee (`u8` variant and varint amount), varint `max_gas`, action (`u8` variant,
    key / byte string / string / asset list / varint amount), signature (`u8` variant: 64 bytes or a byte string). Amounts are LEB128 of `u128` (at most 19 bytes).
  - A key reference is a varint `r`: `r ≥ 1` refers to the `r`-th key in the dictionary (in order of first occurrence in the payload), and `r = 0` is followed by 32 bytes that are appended to the dictionary. The
    bodies and the block's own entries in the same payload share one dictionary.
  - Payload layout: `Settlement` = variant 0, compact headers, list of bodies, list of the block's own entries (**bodies first**, unlike the field order of the type); `Headers` =
    variant 2, compact headers, list of bodies; the list counts and the header indices of the bodies become varints (previously `u32`). `NativeOp` (path C, built by the user's
    wallet) remains plain Borsh encoding, with the chain ID.
  - Payloads no longer implement the Borsh traits: `Payload::encode(&params)` / `Payload::decode(&bytes, &params)` (in TS `encodePayload(payload,
    params)` / `decodePayload(bytes, params)`); encoding rejects a VmOp whose chain ID differs from the network's, and decoding restores the chain ID from the network parameters.
  - Size estimation: the producer and republishing fit entries in by each entry's upper bound (`max_entry_len`: every key other than KSA counted as written in full; a reference is never longer), so any block's body
    can later fit into any payload that carries it; the payload that actually lands on BTC is shorter thanks to dictionary references.
  - Measured: a path B KSA transfer (BIP340 signature) goes from 217 to 143 bytes on first occurrence and 79 bytes when both the account and the recipient are already in the dictionary (slightly below the estimated 145 / 85:
    the varints for `max_gas` and the like are shorter); a forced-inclusion entry stays at 33 bytes.

## D-081 Data moves into the Taproot witness (witness envelope) (M14.3, decided by the project owner; revises SPEC §7.1, §7.2, §17 "data carrier" and D-032, D-034, D-037, D-045, D-047, D-077)

- **Problem**: settlement and republish payloads sit in OP_RETURN at 4 weight units per byte (1 vB); in the Taproot witness each byte counts only 1 weight unit (0.25 vB).
- **Decision**:
  1. **Envelope**: settlement and republish payloads go in the leaf script of a Taproot script-path spend:
     `<funding pubkey> OP_CHECKSIG OP_FALSE OP_IF "KSN1" <payload chunks, each at most 520 bytes> OP_ENDIF`. Path C native transactions still use OP_RETURN
     (built by the user's wallet, small amount of data).
  2. **Commit / reveal transactions**: the commit transaction spends the producer's confirmed funding inputs and has as outputs a P2TR output committing to that leaf (amount = the reveal transaction's fee + 240 sats)
     and change; the reveal transaction spends it, carrying the envelope in its witness, with a 240-sat P2A output (anyone can CPFP). The block carries both transactions (`Block.settlement` becomes
     the commit / reveal pair), and reception checks inputs, signatures, fee rate and `testmempoolaccept` as before (the two as one package).
  3. **Tracker and indexer** scan for the envelope in the reveal transaction's witness; a transaction is recognized only when it has exactly one KSN1 envelope (matching D-032's "exactly one KSN1 output"),
     and OP_RETURN is recognized only for path C native transaction payloads.
  4. Rescue, republishing, delayed broadcast and the funding UTXO pool keep their rules, switched to two transactions; the settlement information in the RPC gives both transactions.
- **Effect** (estimated): 0.25 vB per payload byte; the fixed overhead goes from about 144 vB to about 250 vB (two transactions), so it saves once the payload exceeds about 150 bytes; a path B transfer
  (about 85–145 bytes compressed) is about 21–36 vB.
- **Impact**: `ksana-btc` (envelope, transaction assembly / signing / verification, tracker), `ksana-primitives` (settlement transactions in the block), node (block production, rescue, republishing, broadcast,
  RPC), indexer, TS SDK (RPC types), regtest tests (the relay policy of a real bitcoind) and documentation.
- **Implementation (M14.3)**:
  1. **Why two transactions are necessary**: if the payload were in ordinary witness stack elements (with the leaf script merely dropping them), the funding output could be built in advance and one transaction would do; but
     BIP341 / BIP342 signatures do not cover witness stack elements, so any relayer could swap out the payload while the transaction ID stays the same (a transaction with the same txid but a different wtxid cannot enter a mempool that already has it,
     first come first served), and the producer's settlement could be broken cheaply. The payload must be in a leaf script committed to by an output, hence a commit transaction first to create the commitment output.
  2. **Envelope and recognition** (`ksana-btc::envelope`): leaf script `OP_PUSHBYTES_32 <pubkey> OP_CHECKSIG OP_FALSE OP_IF <magic> <data chunks>... OP_ENDIF`,
     each data chunk at most 520 bytes (pushes in the unexecuted branch are still bound by the 520-byte limit). Recognition is purely syntactic: when the witness, with the annex removed per BIP341, has at least 2 elements,
     the second-to-last is the leaf; one that starts with `OP_PUSHBYTES_32 <32 bytes> OP_CHECKSIG OP_FALSE OP_IF` and a push whose data is the magic (any encoding) is a KSN1
     envelope, and it is well-formed only if the rest is nothing but data pushes (non-shortest encodings accepted) and ends with `OP_ENDIF`. A transaction is recognized only when its KSN1 outputs and envelope-bearing inputs **taken together**
     number exactly one (`data::tx_data`, the carrier being `Carrier::Output` / `Carrier::Input`); the carrier and the payload kind must match: an OP_RETURN output carries only
     `NativeOp`, and the envelope carries only `Settlement` / `Headers` (`data::tx_payload`). Settlement and republish payloads in OP_RETURN are no longer recognized.
  3. **Commit transaction**: inputs as the producer's inputs in D-034; exactly two outputs: 0 change (rules as before), 1 the commitment output -- a P2TR containing only the envelope leaf, with the internal key being the pubkey in the leaf
     (the producer uses the funding key itself, untweaked) and a 33-byte control block (no Merkle path); amount = the reveal transaction's fee + 240 sats.
  4. **Reveal transaction**: exactly one input (output 1 of the commit transaction, empty scriptSig, witness exactly `[signature, leaf, control block]`, signature `SIGHASH_DEFAULT` or
     `SIGHASH_ALL`, verified by the script path); exactly two outputs: 0 a 240-sat P2A (anyone can CPFP, accelerating both transactions at once), 1 a zero-amount `OP_RETURN` with no data
     -- with only one input and the P2A, the non-witness part is 64 bytes, below Bitcoin Core's 65-byte minimum (`tx-size-small`); with it, 74 bytes. The leaf must be the canonical construction
     of the payload generated from the block contents (compared byte for byte), and the control block and the commitment output are both recomputed from the pubkey in the leaf and the leaf and compared byte for byte.
  5. **Fees**: each of the two is ≥ `vsize × MIN_SETTLEMENT_FEERATE`, without relying on package relay; `draft` computes the reveal transaction's fee and the commitment amount from the final witness length,
     and `finalize` recomputes the commitment output from the final payload (same length, so same weight and fee) and then signs the commit transaction first and the reveal transaction second.
  6. **Block encoding**: `Block.settlement: Option<SettlementTxs { commit, reveal }>` (two byte strings in Borsh, `{commit, reveal}` as two
     hex strings in JSON); the shared vectors are regenerated and the TS SDK updated in step.
  7. **Node**: the reception check becomes: the commit transaction's inputs are unspent and the two transactions pass `testmempoolaccept` as one package (`BtcClient::test_accept` now takes a set of
     transactions, parent first; when the commit transaction is already in the mempool or confirmed, only the reveal transaction is submitted for checking); delayed broadcast sends commit then reveal (`broadcast_all`), and settlement records, landing determination and
     mempool checks go by the reveal transaction ID (the transaction carrying the payload), while the funding inputs are the commit transaction's inputs. Rescue settlement and republishing likewise build two transactions (with this node's funding key), with in-flight
     tracking by the reveal transaction; the split transaction for funding UTXOs is still a single one.
  8. **RPC**: `txid` / `txid_rpc` in `ksana_getSettlement` are the reveal transaction, `commit_txid` / `commit_txid_rpc` are added, and `tx` becomes
     `txs: {commit, reveal}`; the `settlement` in block JSON is likewise `{commit, reveal}` (`docs/RPC.md`).
  9. **Measured**: in regtest the maximum 81 920-byte payload is commit 154 vB + reveal 20 710 vB (about 82 060 vB in OP_RETURN); an ordinary settlement is commit
     154 vB + reveal 209 vB (payload 390 bytes). The fixed overhead is about 154 + 111 = 265 vB (estimated about 250 vB), 0.25 vB per payload byte. For a comparison of the same stretch of chain landing on BTC under the M13 and
     M14 rules see `ksana-btc/tests/compression.rs`: one settlement of 120 blocks in 10 minutes drops to 10%–13% of the uncompressed size, and a single-block settlement goes 565 → 351 vB.

## D-082 BTC-anchored finality: a block whose data has `BTC_FINALITY_CONFS` confirmations on BTC is never reorged again (M15, decided by the project owner; revises SPEC §6.4, §10.3, §10.4, §16; the depth of a BTC reorg that takes a final block back corrected by D-114, which also reports a block BTC-final only once the BTC block of its determination has `BTC_FINALITY_CONFS` confirmations)

- **Problem**: fork choice looks only at cumulative work (§6.4). A heavier chain can delay settlement for a long time and be published only afterwards, reorging away blocks whose data landed on BTC long ago and which applications already treat as "BTC-final":
  the node rejects a switch deeper than `REORG_WINDOW`, while the indexer trusts only BTC data and would roll back and rebuild from genesis, so the two diverge; the "BTC-final" of §10.4 is therefore not truly final either.
  The 51% attacker of §16 can roll back to any depth this way.
- **Decision** (agreed by the project owner on 2026-10-03): add a finality rule determined solely by BTC chain data; all nodes and indexers determine it by the same function, and the main chain must pass through the finalized blocks.
  1. **The only input is BTC data**: the block headers that have appeared on BTC (settlement and republish payloads, D-043, D-047) form a header DAG (`ksana-btc::headers`, shared by node and indexer,
     with the same GHOSTDAG rules as D-063; invalid headers and headers whose timestamp exceeds the anchor bound (D-083) do not enter it). The **connected height** `conn(B)` of block B is the lowest BTC height at which the headers of B and all its
     ancestors have landed on chain (`max(seen(B), max conn(parents))`). The **landing height** of a block's data follows the settled-closure rules (D-038, D-077): for a block that needs a body
     it is the confirmation height of the first transaction that carried the same body, otherwise it is the landing height of the header.
  2. **Determination per BTC height**: the finalized tip F starts at the genesis block. For each BTC height H (in order, incrementally): the candidates are the blocks with `conn ≤ H` whose selected chain passes through F; take among them
     the one with the largest `(blue_work, hash)`, best_H; walk up best_H's selected chain from F, and F advances to block B when the data of B's closure (B and the blocks in its merge set) all landed at height
     `≤ H − (BTC_FINALITY_CONFS − 1)` (that is, has `BTC_FINALITY_CONFS` confirmations at H), otherwise it stops. Each step of the determination records the BTC block
     (height and hash) at which the determination was made.
  3. **Pause when undeterminable**: when the data status of some block cannot be determined yet (the node does not have the block: it requests it from peers; the indexer cannot determine the body yet), the determination stops at that height and waits until it is determined;
     BTC blocks after the height already determined do not change the earlier determinations.
  4. **BTC reorg**: the determinations made at the reorged-away BTC blocks are undone and redone along the new branch; the finalized tip may therefore move back, or even switch to another chain (data is final only after 6 confirmations,
     so a shallow reorg almost never changes the result; corrected by D-114: a reorg of just the BTC block that made a determination changes it when the new branch carries a heavier chain).
  5. **The main chain passes through the finalized blocks**: when the node selects the main chain, tips that fork from the main chain below the finalized block do not take part; when this node had selected another, heavier chain before the block became final, it switches to the best tip that passes through
     the finalized block (regardless of work; the forced switch of `Chain::best_candidate`, still bound by `REORG_WINDOW`, alerting when it is exceeded). The block template only merges tips that are not heavier than
     the main-chain tip (existing rule), so block production never leaves the finalized chain. The indexer likewise takes the finalized tip as the floor of its main chain (`HeaderDag::set_floor`), and executes finalized blocks
     without waiting for `SETTLEMENT_DEPTH` confirmations.
  6. **When a finalized block is invalid** (`state_root` mismatch, invalid body), the main chain cannot be reselected: the indexer stops processing and alerts (§10.3 item 5); the node keeps the original main chain and alerts.
     This is the signal that an invalid but finalized chain has appeared on BTC (a software bug or an attack on the network), and needs manual handling.
  7. **RPC**: `ksana_getChainInfo` and `indexer_status` gain `finalized` (the finalized tip); `btc_final` becomes "the main-chain block that accepted this block is on the finalized
     chain", stricter than the previous "the transaction that put it into the settled prefix has `BTC_FINALITY_CONFS` confirmations": it additionally requires that there is no heavier chain on BTC that was published earlier and landed in time.
- **Properties**:
  - Determinism: the determination depends only on BTC chain data and the block contents already on chain, so all nodes and indexers on the same BTC chain get the same answer; disagreement happens only during a BTC reorg and converges along with
    BTC.
  - Security: to reorg a finalized block, the attacker must reorg BTC (at least `BTC_FINALITY_CONFS` blocks) rather than rely on fast-lane hashrate alone; only a chain that lands its data on chain in time can become final,
    and a heavier chain that delays settlement gets "finalized out" by a lighter chain that settles on time -- encouraging timely settlement, consistent with the unlock condition of §8.5.
    Corrected by D-114: a determination rests on no heavier chain having connected by the BTC block that made it, so right after it a reorg of that one BTC block whose new branch carries a heavier chain
    takes the block back; the `BTC_FINALITY_CONFS` blocks hold for a block reported BTC-final, which since D-114 waits for that BTC block to have `BTC_FINALITY_CONFS` confirmations.
  - §6.4 "settlement status never causes a fast-lane rollback" becomes: settlement status does not roll back finalized blocks; a node that selected another chain before the block became final is forced to switch to the finalized chain (which is exactly
    the purpose of this rule). The "fast-lane 51% hashrate attack" of §16 can only roll back blocks not yet final.
- **Cost**: node and indexer each keep a header DAG (all block headers that have appeared on BTC, a little over 100 bytes each); the determination is incremental, and each BTC height only processes the headers that newly appeared
  at that height. `BTC_FINALITY_CONFS` stays at 6 (§12).
- **Implementation (M15)**: `ksana-btc::finality` (determination and rollback on BTC reorg; 5 scenario tests and the property test "incremental determination = determination from scratch"), `ksana-btc::headers`
  (moved in from the indexer's `tree.rs`, with the finality floor added), `tracker.rs` (the snapshot indexes block headers and header times by height; `KnownBlocks::observe_payload` takes the snapshot being
  built), node `chain.rs` (`FinalityState`, `update_finality`, forced switch, `finalized` / `is_final`, awaited blocks are added to `missing_parents`; when an awaited block
  arrives, `accept_block` first parses its payload, then immediately advances the determination and switches the main chain, recording the changes in this report; main-chain changes caused by BTC sync and crash recovery are handed by `take_chain_changes`
  to `ops::apply_chain_changes` to maintain the entry pool, D-041; when the node does not have the block or its payload has not yet been parsed against it, the data status is "undeterminable"), indexer
  `known.rs` / `indexer.rs` (`finalize`, stops when the finalized chain has a block that cannot go on-chain), RPC and TS SDK, `docs/RPC.md`. Tests: node `m15_finality.rs`
  (a late heavier chain cannot reorg finalized blocks, and another node is forced to switch; when both chains settle in time the heavier one becomes final, and a node without it waits and requests it), indexer
  `indexer.rs` (a finalized block pins the indexer's main chain and is executed immediately; an invalid finalized block makes both a running indexer and a freshly started one stop and alert).

## D-083 The timestamp does not exceed the anchor BTC block's time plus `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` (M15, agreed by the project owner; revises the reception check of SPEC §6.2)

- **Problem**: the timestamp is checked against local time only when receiving a new block (§6.2: at most 15 seconds ahead of local time; not checked when syncing historical blocks); the only consensus rule is "greater than the median of the previous 11 blocks".
  Synced historical blocks or a privately mined chain can push timestamps arbitrarily far into the future, and aserti3-2d lowers the difficulty accordingly (timestamp attack); combined with a chain that is withheld for a long time (the problem of D-082) it becomes easier
  to produce a "heavier" chain.
- **Decision**: consensus rule `header.timestamp ≤ time(the header at height btc_anchor.height on the current BTC best chain) + MAX_TIMESTAMP_AHEAD_OF_ANCHOR`,
  parameter `max_timestamp_ahead_of_anchor_secs = 14 400` (4 hours, the same on devnet and signet). When the anchor block is on the best chain, that is the anchor block itself (the anchor hash
  already commits to it); when the anchor block has been reorged away by BTC, the block at the same height on the current chain is the answer, consistent with D-056. The time is taken only from the BTC tracker's snapshot, never asking bitcoind, so node and
  indexer reach the same conclusion for the same BTC chain: it is a deterministic function of BTC chain data. The node checks both when receiving new blocks and when syncing historical blocks (`Chain::accept_one`; a block that fails is recorded as
  an invalid block), the producer checks once more when committing its own block (`commit_block`), and the header DAG (the finality determination of indexer and node) checks too. When the snapshot does not yet have the anchor height
  (the local BTC view is behind the anchor) there is no check -- such a block cannot have its forced-inclusion window derived per §7.4 anyway, and is handled by the anchor check.
- **Producer**: the timestamp is `min(max(local time, median time + 1), bound)`: when the local clock is ahead or BTC has had no new block for a long time, it is clamped to the bound; when the bound is no longer above the median time,
  no block is produced and the producer waits for BTC to advance (after BTC stalls for more than about 4 hours the fast lane pauses with it, instead of producing a block that everyone else rejects).
- **Why 4 hours**: the anchor is the BTC tip at production time (which may lag by `ANCHOR_DEPTH`, D-056), BTC block intervals have a long tail, and BTC header times are allowed to be about
  2 hours behind real time (the MTP rule), so an honest producer's timestamp normally does not exceed the anchor time by more than 3 hours; the smaller the bound, the more dangerous for honest nodes; the larger, the more difficulty discount an attacker can borrow. With 4 hours,
  each anchor period can borrow at most 4 hours of "ahead" time, and it does not accumulate (the next anchor restarts the count).
- **Impact**: a new `Params` field (not in the encoding), `ksana-consensus::check_anchor_time`, node (`Chain::anchor_time` taken from the snapshot) and indexer, the header
  DAG's check, the producer's timestamp bound and pause, and the mock BTC chain's clock starting from the system time at creation (otherwise fast-lane timestamps would exceed the regtest genesis block's 2011 time plus
  4 hours). Tests: `a_block_too_far_ahead_of_its_anchor_time_is_invalid` in `m15_finality.rs` (valid exactly at the limit, invalid 1 second past it and recorded as an invalid block),
  `timestamps_are_bounded_by_the_anchor_time` in `headers.rs`.
- **Revision (M15 self-review)**: the first draft took the time from "the BTC header the anchor hash points to" and asked bitcoind outside the snapshot's range: for a reorged-away anchor block the nodes' answers differed (depending on whether bitcoind knows it
  and whether it still has the header), and syncing historical blocks took one RPC per block. After changing to "the block at the anchor height on the best chain" above, the conclusion depends only on BTC chain data.
## D-084 Two-speed difficulty: an empty block is N times harder, has weight N and N reward slots (M15, the project owner decided N = 12; revises SPEC §6, §6.3, the rewards of §8.6 and §13)

- **Problem**: when the chain is idle, every block interval still produces an empty block whose header (compacted) must also land on chain yet contributes nothing to the state; but when there are entries we still want to respond at the pace of one block
  every 5 seconds. A single block interval cannot achieve both "land less on chain when idle" and "fast when there are entries".
- **Decision**: the difficulty differs by whether the block is empty (two-speed difficulty), N = `EMPTY_BLOCK_DIFFICULTY` (a new protocol parameter, 12 on devnet and signet;
  0 is treated as 1), with identical rules in node, indexer and the shared crates:
  1. **Weight**: `w(B) = N` when `B.entries_root` equals the root of the empty entry list (no entries), otherwise `w(B) = 1`; the genesis block has weight 1 and DAA score 0 by definition.
  2. **DAA score**: `D(B) = D(sp) + Σ_{m ∈ mergeset(B) \ {sp}} w(m) + w(B)` (both blue and red blocks count). The protocol version (D-076),
     `BlockEnv.rules` and the block reward all follow `D(B)`; `bits` is independent of `w(B)`.
  3. **bits**: ASERT's `time_diff` is still the selected parent's timestamp minus the anchor point's time, and `height_diff = D(B) − w(B)`, so an empty and a non-empty block with the same parents have
     the same `bits`; `Consensus::next_bits(sp, D(B), w(B))`.
  4. **PoW target and work**: effective target = `target(bits) / w(B)` (integer division); the header's PoW is `hash ≤ effective target`; when the effective target is 0
     (`bits` too small) the header is invalid (an error is returned, no panic). `work(B) = ⌊2^256 / (effective target + 1)⌋` (blue work and main-chain selection both use it),
     and `solve` solves against the effective target. The genesis block's work is likewise computed from the effective target (24 instead of 2 for the devnet genesis block): it is never in any merge set, so consensus is unaffected.
  5. **Header version and rules**: `header.version == protocol_version(D(B))`, `BlockEnv.rules = rules(D(B))`, where `D(B)` includes the block's own weight.
  6. **Reward**: `reward(B) = Σ_{i=0}^{w(B)−1} block_reward_at(D(B) − i)` (saturating sum; slots with negative `D(B) − i` do not count), still paid at unlock
     (D-079) and capped by `KSA_MAX_SUPPLY`; red blocks still get no reward. The formula of the total reward bound `max_total_rewards` is unchanged (one slot per DAA score);
     the slots of parallel blocks may overlap, and the supply cap is the backstop, as in D-074.
  7. **Producer**: the template computes GHOSTDAG with weight 1; when there are entries to include (forced inclusion, unlock or candidates) it is prepared as a non-empty block (version and rules per the `D` that includes weight 1),
     otherwise as an empty block (weight N); when there are still no entries after including with weight 1, it is redone once as an empty block (the rules may differ), and an empty block no longer tries to include entries, so the weight after the redo
     always matches the plan. Solving uses the effective target; when a new op enters the entry pool the empty-block template is voided immediately (already covered by the existing entry-pool generation check).
- **Cost**:
  - When idle, a header lands on chain only about every N block intervals (about 60 seconds on devnet); the "heartbeat" of one header every 5 seconds is gone.
  - Rewards are paid per DAA-score slot: empty and non-empty blocks have the same expected reward per unit time, so there is no incentive to churn out empty blocks; the sum of rewards paid per block may still slightly exceed the bound summed by score
    (already the case in D-074), capped by the supply.
  - A producer can stuff one op of its own into its block (the path B fee is paid to itself) to turn it into a weight-1 non-empty block, producing one block every 5 seconds and putting the body
    on chain: this only costs its own BTC settlement fees and earns no extra reward (slots follow the DAA score); for others it only adds the on-chain data of headers and bodies and the indexing cost; "land less on chain when idle"
    is an incentive for honest producers, not an adversarial guarantee.
  - When idle, each block takes about N × 5 seconds (about 60 seconds on devnet) while the ASERT half-life is 600 seconds: each block's random duration moves the target by about 7%, the difficulty jitters by about ±20%, and the time per unit of DAA
    score still converges to 5 seconds (`empty_blocks_converge_to_n_times_the_spacing` in `convergence.rs`). `EMPTY_BLOCK_DIFFICULTY` is
    a field of `Params`, not in the upgrade schedule's `Rules`: changing N is a hard fork.
  - Tests: with empty-block weight N, ASERT's schedule advances N intervals per block, so a test chain producing blocks at millisecond intervals runs far ahead of schedule and its difficulty doubles about every 10 empty blocks (in the node tests, the
    `pow_rate` speed measurement would also fail outright if it used empty-block headers, since target 1 / 12 = 0). The genesis timestamp of in-process tests and regtest tests is moved 30 days earlier (the chain is always
    behind schedule and the difficulty stays at the PoW limit; scenarios with a given genesis difficulty are not moved), the `block_interval` test now submits transfers continuously and computes the expected duration from each block's weight,
    and `path_b` / `restart` switch to the PoW limit (the block rate is limited by the minimum interval). devnet keeps N: within about 20 minutes after startup the empty-block interval rises from 5 seconds to about 60 seconds,
    and the time limits of `smoke.sh` / `soak.sh` are unaffected.
- **Impact**: `ksana-primitives` (`Params::empty_block_difficulty`, `empty_block_weight`, `block_weight`, `block_rewards_at`),
  `ksana-consensus` (`weight` / `next_bits` / `check_header` of the `Consensus` trait, `effective_target`, `check_pow` takes the weight,
  GHOSTDAG's `DagBlock.weight` and `BlockDag::compute` / `insert` take the weight, new error `TargetTooSmall`), `ksana-btc` (`HeaderDag`,
  `weight` of `BlockSummary` / `MainEntry`), `ksana-execution` (`UnlockItem.weight`, rewards paid per slot at unlock), node (DAG, template, producer,
  the RPC's pending rewards), indexer, tests and docs. The Borsh encoding of headers, entries and payloads is unchanged, the shared vectors are unchanged; the RPC and TS SDK types are unchanged
  (the meaning of `daa_score` is extended to accumulate by weight).
- **Implementation (M15)**:
  1. `Params::block_weight(header)`, `block_rewards_at(daa_score, weight)`; `Consensus::weight(header)`,
     `next_bits(sp, daa_score, weight)`, `check_header(sp, daa_score, header)` (`daa_score` includes the block's own weight), `work` / `solve` against
     the effective target; `chain_next_bits` / `check_chain_header`, which applied only to linear chains, are removed (tests roll their own with "DAA score equals height").
  2. `BlockDag::compute(parents, weight)` / `insert(hash, parents, work, weight, payload)`; node `Chain` (`check_dag_header`,
     `commit_block`, `load_dag`, the template uses weight 1 plus `Template::daa_score(weight)`) and `ksana_btc::headers::HeaderDag::accept` all pass
     `consensus.weight(header)`.
  3. Unlock: `UnlockItem.weight` comes from the DAG (`BlockSummary` / `MainEntry`); the execution layer pays rewards per `block_rewards_at(daa_score, weight)`, and a block in the segment
     whose weight is neither 1 nor N is a local fault (`Fatal`); the pending rewards in the node RPC `ksana_getAccount` likewise count slots.
  4. Producer: `bits = next_bits(tip, template.ghostdag.daa_score, 1)`, version and `BlockEnv.rules` per the `D` of the planned weight, redo once when the inclusion result
     disagrees with the plan, and before committing verify that the header's weight matches the plan; the log prints the weight.
  5. Tests: consensus (effective target / work / solving for empty and non-empty blocks, `bits` independent of weight, an empty block that meets only the non-empty target is invalid, target too small, GHOSTDAG weight
     accumulation, the time per unit of DAA score still converges to 5 seconds with only empty blocks or a mix), execution layer (N-slot rewards, across a halving point, an abnormal weight is a local fault), node (`m9` rewards
     per slot, `m11` judges activation and the fork height by DAA score, `block_interval` with continuous transfers), indexer (forged blocks compute the DAA score by weight).
     The `determinism.rs` scenarios have no unlock entries, so the golden roots are unchanged.
## D-085 The exchange example changes to a lock flow (M15, the project owner decided: the exchange is only a VM example and does not enter the core protocol; the flow is order → lock → confirm → redeem; revises D-078)

- **Problem**: the example exchange contract of D-078 is a "posted order + collateral balance" model: a sell order's balance is shared by all buyers, whoever's payment confirms first takes first, the shortfall is recorded as a seller
  debt to be repaid when the seller adds collateral, and the seller can close the order or withdraw the balance at any time. When paying BTC, the buyer gets no guarantee that "this KSA will certainly be mine": another payment to the same sell order
  may confirm first, or the seller may withdraw the balance before the payment confirms, leaving the buyer with only a debt; the debt queue, deduplication by transaction ID and the order-closing rules also complicate the state machines of the contract, SDK and CLI.
- **Decision**: change to the four steps "order → lock → confirm → redeem". Only the example contract, SDK, CLI, tests and docs change; the core crates are unchanged (the host functions `native_tx` and
  `btc_anchor_height` suffice; the ABI, gas table and shared vectors are untouched).
  1. **Seller funds**: `deposit()` (with attached KSA) goes into the seller's free balance in the contract; `withdraw(amount)` can only take from the available balance and takes effect immediately. Sell orders and the debt
     queue are removed: `close` / `list` / `order` / `order_of_script` / `debts` / `claimed` / `quote` no longer exist.
  2. **Lock** `lock(ksa, price, script, beneficiary, expiry) -> lock_id` (u64, incrementing from 0): moves `ksa` from the available balance (KSA attached to the call
     is credited to the available balance first, so the seller's first lock can be a single path B op) into the lock. Requires `ksa > 0`, `price > 0` (how many KSA base units per sat),
     `expiry > btc_anchor_height()`, a receiving script of 1–64 bytes that is not OP_RETURN and has **never been used by any lock** (the script → lock id mapping is kept
     forever, any payment can correspond to only one lock, and the seller uses a new receiving address for each lock). The beneficiary is fixed when locking and the terms cannot be changed. State `Open` →
     `Claimed { paid_sats, ksa_paid }` or `Released`.
  3. **Confirm (claim)** `claim(lock_id) -> (paid_sats, ksa_paid)`: accepts only path C (`native_tx` must exist), sums the amounts of the transaction's outputs paying `lock.script`,
     requires `paid_sats > 0` and the lock to be `Open`; `ksa_paid = min(paid_sats × price (saturating multiplication), lock.ksa)` is transferred to the beneficiary,
     the unsold part returns to the seller's available balance, and the lock is marked `Claimed`. The claim op carries only the lock id; the caller (a one-time account) has no bearing on where the KSA goes, and copying or replaying it
     cannot change the beneficiary either. Claiming a lock that is not `Open` fails (the path C nonce is still consumed, D-018); SDK and CLI refuse before paying.
  4. **Redeem** is simply the KSA being credited to the beneficiary when the claim succeeds; there is no separate redeem call; the "order" step agrees amount, price and buyer account off-chain.
  5. **Release** `release(lock_id) -> ksa`: only the seller, with the lock `Open` and `btc_anchor_height() ≥ expiry + RELEASE_DELAY`
     (`RELEASE_DELAY = 6`); the KSA returns to the available balance, state `Released`. Claiming does not check expiry: a payment confirmed after expiry but before release can still be claimed; the extra wait of 6 BTC blocks
     leaves time for a payment confirmed close to expiry to be force-included (the native transaction's confirmations and the lag of the node's anchor height both fit within it).
  6. **Queries**: `get_lock(lock_id) -> Lock` (the action is already named `lock`, so the query gets a prefix to avoid the same name), `lock_of_script(script) -> Option<u64>`,
     `balance(seller) -> u128`, `lock_count(seller) -> u64`, `locks_of(seller, start, limit) -> Vec<u64>` (`limit` 1–256; a per-seller
     index of lock ids, one extra storage write per lock). Borsh field order of `Lock`: `seller: [u8; 32]`, `beneficiary: [u8; 32]`, `script: Vec<u8>`,
     `price: u128`, `ksa: u128`, `expiry: u32`, `state: LockState` (`u8` variants: 0 `Open`, 1 `Claimed { paid_sats: u64, ksa_paid: u128 }`,
     2 `Released`).
- **Cost**: the KSA is locked before the buyer pays; the seller locks separately for each buyer and bears the opportunity cost of not being able to use it before expiry; a receiving script can be used only once; an overpayment buys no
  extra KSA (SDK / CLI refuse overpayment by default, `allowOverpay` / `--allow-overpay` allow it), and an underpayment buys only a part while the lock closes right away (refused by default, `allowPartial` /
  `--allow-partial` allow it); before paying, the buyer must verify that the lock's beneficiary is itself (SDK and CLI require the buyer account and compare it, refusing otherwise), that the lock is still `Open`
  and that expiry is far enough away (the SDK's default margin is 3 BTC blocks; the `payBy = expiry + RELEASE_DELAY − NATIVE_CONFIRMATIONS − 1` returned by `buildTopUp` is the height before which the payment
  must be confirmed in a BTC block: the entries of merged blocks execute before the block's own forced-inclusion entries (D-063), so if a block whose anchor jumps straight to `expiry + RELEASE_DELAY`
  merges a side-chain block carrying a `release`, the release executes first).
  **Known attack** (self-review 2026-10-03): the claim op's signature does not cover the BTC transaction that carries it. After seeing the buyer's payment transaction in the BTC mempool, the seller (or anyone) copies the
  signed claim op from it into a transaction of their own that pays nothing and carries a higher fee; when it confirms first, the claim reverts (`paid_sats` is 0) yet consumes nonce 0 of the one-time key, and when the buyer's
  real payment confirms afterwards the claim is no longer valid: the seller gets the BTC for free and can still release the KSA after expiry. This has been a general flaw of path C since D-078, and the lock flow does not change it; the project owner
  decided on 2026-10-03 to fix it at the protocol layer -- the signature of a path C op is bound to the first input of the carrying transaction (D-086, also implemented in M15; this attack is no longer feasible).
- **Impact**: `contracts/examples/ksa-exchange` (contract and mock-host tests), `ksana-execution/tests/native_tx.rs`, TS SDK `topUp.ts`
  (`ExchangeLock` / `decodeLock` / `encodeLock`, `lockArgs`, `claimArgs`, `getLock`, `quoteTopUp`, `buildTopUp` and its refusal conditions:
  not `Open`, overpayment, `currentBtcHeight + expiryMargin ≥ expiry`) and `top-up.test.ts`, `ksana-cli` (`top-up --lock`, new
  `exchange deposit / withdraw / lock / release / show / balance`), the `top-up` of `regtest.ts` (the seller's mock wallet locks via path B first) and
  the M13 end-to-end test, the READMEs of the SDK and the demo page, `docs/SIGNET.md`. SPEC, ABI, core crates and shared vectors are unchanged.
- **Implementation (M15)**: storage keys `next` (next lock id), `l‖id` (lock), `s‖sha256(script)` (script → lock id), `b‖seller` (available balance),
  `n‖seller` (lock count), `k‖seller‖index` (lock id index); events `deposit` / `withdraw` / `lock` / `claim` / `release`. The CLI's
  `top-up` uses bitcoind's tip height for the expiry check (`--expiry-margin`, default 3); in the SDK the caller passes `currentBtcHeight` (the wallet's BTC tip when it has one,
  otherwise the anchor height of the node's latest block, a looser check).

## D-086 The signature of a path C op is bound to the first input of its carrying transaction (M15, decided by the project owner; revises D-018, D-036, D-078)

- **Problem**: the signature of a path C op covers only the VmOp itself (the original text of SPEC §4.2) and is independent of the BTC transaction containing it. Anyone who sees the signed op can copy it verbatim into
  a BTC transaction of their own: for ordinary ops this merely pays someone else's fee, but for the "op and payment in the same transaction" usage (the exchange example's `claim`, D-085) it steals the payment --
  when the copy confirms first, the claim reverts but consumes nonce 0 of the one-time key, and when the buyer's real payment confirms afterwards the claim is no longer valid (the known attack recorded in D-085). The contract layer
  cannot stop it (a contract cannot tell whether an op was copied), and the SDK could only advise "do not publish before the payment confirms".
- **Decision** (the project owner, 2026-10-03, chose protocol binding between "contract-layer deduplication by payment transaction" and "protocol-layer binding"): for an op whose `fee` is `BtcNative`, the signing
  message is `encode_unsigned(op) ‖ txid ‖ vout`, where `txid ‖ vout` (32 bytes in internal byte order + u32 little-endian, 36 bytes in total, `OutPoint::to_bytes`)
  is **the output spent by the first input** of the carrying transaction; the tag is still `Ksana/VmOp/v1`, and the path B message is unchanged. The `Bip322Simple` message is likewise the hex of the bound
  sighash. The execution layer verifies the signature against the carrying transaction (`BtcView::native_first_input`, supplied by the node's / indexer's BTC view; a failed lookup is a local fault `Fatal`): an op copied
  into another transaction fails authentication and is recorded as failed without consuming the nonce (the same as an invalid signature; a revision of D-018). A coinbase transaction has no real input and is no longer recognized as
  a native transaction (a revision of D-036; "coinbase applies too" was previously used only in tests).
  - Why bind the outpoint rather than the transaction ID: the transaction ID covers all outputs, and the data output itself contains this signature, a circular dependency. An output can be spent only once on the whole BTC chain,
    so the "first input" is unique on the chain (except for coinbase), consistent with the commitment to inputs in BIP341 / BIP143. Binding only the first input instead of all inputs lets the signature be fixed
    before the wallet's coin selection (adding inputs); the cost is that the first input has to be fixed first when constructing.
  - Why not deduplicate by payment transaction at the contract layer: a contract cannot see "whether this is the buyer's own transaction"; deduplication also cannot stop a non-paying copy that confirms first from consuming the nonce.
- **Cost**: constructing path C takes one more step: pick the first input, then sign. SDK: sign with `nativeFirstInput(utxos[0])` as `firstInput`, and `buildNativeTx`
  rejects an op not bound to `utxos[0]`; the wallet's BIP322 signing request must therefore be issued after coin selection. The CLI and the integration tests fund with the bitcoind wallet, and `fundrawtransaction`
  does not accept "inputs fixed first", so they first fund with a placeholder op (signed against an all-zero outpoint, same encoded length; the ECDSA signature of BIP322 P2WPKH has variable length, so the placeholder payload reserves 2 extra bytes),
  take the first input and sign, replace the data output with the real op (`with_native_op`), and then have the wallet sign each input (the input signatures therefore cover the final data output).
  Path C ops carried in a coinbase are no longer recognized. This is a consensus change: the signing message, the recognition rule and the shared vectors all change, and devnet / signet restart.
- **Impact**: `ksana-primitives` (`OutPoint::to_bytes`); `ksana-crypto` (`vmop.rs` splits the interface into two groups by fee mode: `vmop_sighash` / `sign_vmop` /
  `sign_vmop_bip322_*` / `verify_vmop` accept only path B, `native_vmop_sighash` / `sign_native_vmop` / `sign_native_vmop_bip322_p2tr` /
  `sign_native_vmop_bip322_p2wpkh` / `verify_native_vmop` accept path C; using the wrong one gives `SigError::FirstInputRequired` /
  `UnexpectedFirstInput`); `ksana-btc` (`NativeTx.first_input`, `native::first_input` / `outpoint_of` / `with_native_op`, recognition excludes
  coinbase, `AnchoredView::native_first_input`); `ksana-execution` (`BtcView::native_first_input`, `forced_prepare` verifies signatures against it);
  `ksana-cli` (two-step funding in `send_native`, `submit` and `top-up`); integration tests (`Btc::fund_native`, the replay scenario in `m1_regtest` now has the copy fail with
  "invalid signature" without consuming the nonce, `m0_regtest` verifies signatures against the transaction read back from the chain); fuzz targets and seeds; TS SDK (`vmopSigningMessage` /
  `encodeOutPoint`, `signVmop` / `signedVmop` / `verifyVmop` / `bip322Message` / `signVmopBip322` / `verifyVmopBip322` /
  `firstInput` of `signVmopWithWallet`, `nativeFirstInput` / `nativeTxFirstInput` / `isBoundTo` in `pathC`, `buildTopUp` binds
  `utxos[0]` automatically, `nativeOpFromTx` returns `null` for coinbase); shared vectors (the path C cases in `signing.json` carry `first_input` and the bound
  `sighash`, `codec.json` / `merkle.json` regenerated along with the signatures); SPEC §4.2 / §7.4, CLAUDE.md, the SDK README, the D-036 entry of PENDING_REVIEW.
  The ABI is unchanged (the return value of `native_tx` is unchanged); the code of the exchange example contract is unchanged, and the known risk is removed from its documentation.
- **Implementation (M15)**: `crates/ksana-crypto/src/vmop.rs`, `crates/ksana-btc/src/native.rs`, `crates/ksana-execution/src/executor.rs`
  (`forced_prepare`). Tests: `native_signatures_are_bound_to_the_first_input` in `vmop_sig.rs` (a different transaction, a different vout, no outpoint, and path B
  with an outpoint all fail), `vectors.rs`, `ksana-btc/tests/native.rs` (coinbase is not a native transaction, `first_input`, `with_native_op` replaces only the data output),
  `anchored_view.rs` (`native_first_input` inside and outside the snapshot and when anchor-invalid), the execution-layer test view `TestBtc::first_inputs` (by default derived from the transaction ID,
  `test_first_input`), the node and indexer mock-chain tests fund first and then sign, `m1_regtest` / `m0_regtest`, TS `signing.test.ts` (including the BIP322
  binding) / `path-c.test.ts` / `top-up.test.ts` / `wallet.test.ts`.

## D-087 Form of the fast-lane PoW: no merged mining; two-speed difficulty and "one's own transactions" stay as they are (after M15, decided by the project owner 2026-10-03; revises SPEC §15, §16, §17)

- **Problem**: SPEC §17 "Long-term form of fast-lane consensus" listed three alternatives, of which (a) merged mining with BTC mining pools (AuxPoW) was written into §16 as the long-term answer to the 51% hashrate attack.
  The chain design review (2026-10-03) pointed out that it is incompatible with "all headers on-chain": an AuxPoW block has to carry the BTC block header, the coinbase and the Merkle branch as its proof of work
  (about 700–900 bytes), and since the indexer has to verify the PoW and the chain selection independently from the data on BTC, this proof would have to go on-chain together with the header, tens of times the size of each empty header today (14–18 bytes compressed);
  it would only be feasible after switching to "only data blocks go on-chain and BTC order is finality". The remaining-issues list after M15 has two more items related to two-speed difficulty (D-084): the speed of recovery
  from empty blocks to non-empty blocks, and a producer stuffing one of its own ops into its block to turn it into a weight-1 non-empty block.
- **Decision** (the project owner, 2026-10-03):
  1. No merged mining (Doge / LTC-style "dual mining" is AuxPoW, the same thing). The only alternatives left in §17 are electing producers by spending on BTC, and PoS; the default is unchanged:
     independent SHA256d PoW (GHOSTDAG) + BTC-anchored finality (D-082).
  2. Two-speed difficulty is unchanged. The requirement is "as soon as there is a single transaction, return to normal speed immediately", and the status quo already satisfies it: while mining an empty block the producer checks the entry pool every `SOLVE_CHUNK` (16384) hashes,
     and when a new op arrives it discards the empty-block template and produces a block again at the target of `bits` (`producer.rs`, the "producer" paragraph of D-084), so the first transaction waits for exactly one normal block (expected 5 seconds,
     subject to ASERT's roughly ±20% jitter); without transactions the next block is an empty block. The roughly 20-minute difficulty ramp-up after devnet starts is the transition from the lowest genesis difficulty, during which empty blocks
     can only be too fast, which has nothing to do with transactions; mainnet / signet do not have this phase when the genesis sets the difficulty.
  3. No rules such as "a non-empty block must contain at least one op from someone else" or "a minimum total fee": one's own transactions are transactions too. "Less on-chain when idle" is an incentive for honest producers, not an adversarial guarantee
     (already recorded in D-084).
- **Rationale**: see "Problem". Besides, without merged mining, existing BTC ASICs cannot mine this chain's blocks directly either: the PoW value is `sha256d(borsh(header))`, the header is variable-length and the nonce is
  u64, not the 80-byte BTC layout (D-062). Letting ASICs mine directly in the future would need a new header version (an 80-byte mining header committing to the real header), and that would be miners moving
  hashrate over from BTC and having to be paid more per hash than BTC, not the zero marginal cost of merged mining.
- **Impact**: documentation only. "Long-term form of fast-lane consensus" in SPEC §17 drops merged mining and notes that it is ruled out; the description of electing producers by BTC spending gains the Stacks Nakamoto-style
  key points (registering and paying on BTC every BTC block cycle, a lottery weighted by the BTC block hash, the winner produces blocks exclusively for one cycle, security comes from spending on BTC; empty blocks, headers on-chain and
  D-084 would then be unnecessary, D-082 unchanged; the costs are the liveness and censorship of a single producer, producers that must keep spending BTC, and a rewrite of the consensus layer); the 51% attack row in §16 now points to §17;
  the wording of "After M16" in §15 is adjusted accordingly. Code, parameters and shared vectors are unchanged. The current advice is to run on the frozen PoW first and consider changing the form only if an actual hashrate attack occurs.

## D-088 Block production is settled as PoW (PoS ruled out); the N of two-speed difficulty and the ASERT jitter stay per-network parameters, not in the upgrade schedule (decided by the project owner 2026-10-03; revises SPEC §13, §14, §16, §17)

- **Problem**: after D-087, SPEC §17 "Long-term form of fast-lane consensus" still listed two alternatives (electing producers by BTC spending, PoS). The remaining-issues list after M15 also asked: the two-speed difficulty
  multiplier `EMPTY_BLOCK_DIFFICULTY` (N = 12) is a `Params` field and not in the upgrade schedule's `Rules`, so changing N is a hard fork; should it be moved into `Rules` before the freeze; when idle each block takes about
  N × 5 seconds while the ASERT half-life is 600 seconds, giving a difficulty jitter of about ±20%; should the half-life be scaled by weight. The project owner also asked whether SHA256d is quantum-resistant.
- **Decision** (the project owner, 2026-10-03 21:31 UTC):
  1. "block production is definitely PoW; which form of PoW exactly is not decided yet": block production is PoW, and PoS and electing producers by BTC spending (neither is PoW) are no longer listed as alternatives. The concrete form
     (keep SHA256d on our own hashrate; merged mining with BTC pools; an 80-byte mining header so ASICs mine directly, D-062) is decided together with "whether empty headers keep going on-chain" (§17).
  2. "no need": N and the jitter do not go into the upgrade schedule. `EMPTY_BLOCK_DIFFICULTY` and `ASERT_HALF_LIFE` are both per-network fixed `Params` fields, set at genesis; changing them means a new network
     (devnet / signet restart); the half-life is not scaled by weight, and the ±20% jitter is accepted (D-084's analysis: the time per unit of DAA score still converges to 5 seconds regardless of transactions).
  3. SHA256d and quantum computing (answered in the thread, recorded here for reference): against quantum computing, hash-based PoW faces only the square-root speedup of Grover's algorithm -- finding a hash that meets the target goes from about 1/p attempts
     to about √(1/p), and every step has to compute the full SHA256d in a quantum circuit and bear the error-correction overhead; existing resource estimates all conclude that quantum miners will not catch up with
     ASICs in the foreseeable future; this speedup is the same for any hash algorithm (SHA256d, Blake3, RandomX, ...), so "switching to another hash" would not be more quantum-resistant, and SHA256d is not the weak point. What is really not quantum-resistant is
     the secp256k1 signature (Shor's algorithm): this chain's BIP340 / BIP322 signatures, the P2TR / P2WPKH inputs of settlement transactions, the BTC transactions of path C, and BTC itself are all affected.
     This does not depend on this chain: when the time comes, the VmOp signature type and the settlement input scripts are switched via the upgrade schedule (§6.9) following BTC's post-quantum signature scheme (BIP-360 or the like); the PoW need not change.
     The choice of PoW form therefore depends only on where the miners come from and the on-chain cost, not on quantum.
- **Rationale**: a parameter set once at network launch needs no activation path; moving N into `Rules` would require every place that uses weights (the GHOSTDAG of the node and the indexer, `ksana-btc`'s
  header DAG and finality, the weights of unlock segments, the TS SDK) to look up the rules by DAA score, and the complexity is not worth it. The same goes for the half-life. The reason PoS is ruled out is in the discussion around D-087: security comes from
  BTC and hashrate, and the distribution of KSA should not decide who produces blocks.
- **Impact**: documentation only. "Long-term form of fast-lane consensus" in SPEC §17 is rewritten as "Concrete form of the fast-lane PoW" (PoS and election by BTC spending removed, the three PoW forms and the conclusion on the quantum
  question listed), and a new row "Whether empty headers keep going on-chain" points to the design note; the "Mainnet parameters" row notes that N and the half-life do not go into the upgrade schedule; the two parameter rows in §13 get a note; "After M16" in §14
  and the 51% attack row in §16 are adjusted accordingly. Code, parameters and shared vectors are unchanged.

## D-089 KSA economic parameters: a supply of 210 million, halving about every two years, a block share (decided by the project owner 2026-10-03, the recipient 2026-10-04; revises D-074 and SPEC §6.9, §8.6, §10.1, §13, §17; block share implemented in M16; the halving periods and `BLOCK_REWARD` revised by D-105, the share's end point unchanged; mainnet's block share left out of the published source by D-134)

- **Problem**: the issuance figures of D-074 are placeholders (a supply of 1 billion, 9.9 KSA per slot, halving every 4 years, a genesis allocation of up to half), listed in SPEC §17 as pending confirmation. The project owner, 2026-10-03
  21:31 UTC: "a supply of 210 million, halving every two years, a block share [its size and length not published, D-134], whether there is any afterwards to be discussed separately". The recipient of the share was not stated.
- **Decision** (mainnet consensus parameters; the devnet / signet values are set separately at implementation time, as the tests need a genesis allocation):
  1. **Max supply** `KSA_MAX_SUPPLY` = 210 million KSA = 2.1 × 10^16 base units (precision 8 unchanged).
  2. **Halving interval** `REWARD_HALVING_INTERVAL` = 12 500 000 units of DAA score. At the planned 5 seconds per unit of DAA score that is about 723.4 days (1.98 years); this round number is chosen so that
     `2 × BLOCK_REWARD × interval` is exactly the supply and every period and the block share's split are integers. Exactly two years would be 12 614 400, corresponding to 8.32381984 KSA per slot and a total of 209 999 984.59 KSA;
     this set can be used instead if the project owner requires exactly two calendar years. Halving is still by DAA score (D-074): an empty block occupies N slots by weight (D-084), so whether the chain is idle or not does not change the issuance per unit of time.
  3. **Reward per slot** `BLOCK_REWARD` = 8.4 KSA (840 000 000 base units), `reward(d) = 840 000 000 >> ⌊d / 12 500 000⌋`. The sum over all slots is 209 999 999 KSA
     (the truncation of the integer right shift loses 1 KSA), and it is 0 after the 30th halving (around year 59). `max_total_rewards = 2 × 8.4 × 12 500 000 = 210 million`, so
     `max_genesis_supply` is 0: mainnet has no genesis allocation (a fair launch; the block share is the only allocation at the protocol layer). If the project owner wants a genesis allocation, `BLOCK_REWARD` is lowered proportionally
     (0.084 KSA less per slot for every 1% of the supply allocated).
  4. **Block share**: for every reward slot with DAA score `d < BLOCK_SHARE_END`, `reward(d) × BLOCK_SHARE_BPS / 10 000` (rounded down) is credited to the block share account and the rest goes
     to the producer; mainnet's `BLOCK_SHARE_BPS` and `BLOCK_SHARE_END` [not published, D-134]. Later slots go entirely to the producer. "whether there is any afterwards to be discussed separately": the share ratio and the cutoff are made rules that vary with the protocol version (`Rules`, §6.9), so if continuing is decided later, adding an entry to the upgrade schedule suffices,
     with no hard fork. The share applies only to block rewards, not to the sponsored fees of path B (default; confirmed on 2026-10-05 with the consensus freeze, D-096).
  5. **Recipient**: decided by the project owner on 2026-10-04 [not published, D-134]: an ordinary account given in the genesis config
     (`block_share_account`, an x-only public key, part of the config fingerprint like the allocations); when multi-party management or on-chain rules are needed, the balance can be transferred to a contract
     account. Alternatives not adopted: splitting among several accounts at fixed ratios, or receiving directly with a contract account (the genesis state currently has no contracts, so genesis deployment would have to be added first).
  6. **Payout**: still at unlock time (§8.5, D-079): each rewarded position splits its slots into a share part and a producer part, credited to the available balances of the two accounts respectively, and `total_supply` is increased by the sum
     of both; the supply cap logic is unchanged (the two parts together do not exceed `KSA_MAX_SUPPLY − current total supply`, which only triggers far in the tail; when a block can only pay out part, the share's portion is guaranteed first
     and the producer takes the remainder). When the genesis config gives no share account, everything goes to the producer. Executing an empty block still writes no state. The `locked_fees` of `ksana_getAccount` is estimated for the producer by its
     own portion only, and for the share account lists the portion each block will share with it.
  7. **Emission schedule** (by DAA score; "years" are converted at 5 seconds per unit and are only approximate):

     | Halving period | DAA score | Roughly | Reward per slot | Issued in period | Cumulative (of supply) |
     | --- | --- | --- | --- | --- | --- |
     | 0 | 0 – 12 499 999 | years 0–2 | 8.4 KSA | 105 million | 105 million (50%) |
     | 1 | 12 500 000 – 24 999 999 | years 2–4 | 4.2 KSA | 52.5 million | 157.5 million (75%) |
     | 2 | 25 000 000 – 37 499 999 | years 4–6 | 2.1 KSA | 26.25 million | 183.75 million (87.5%) |
     | 3 | 37 500 000 – 49 999 999 | years 6–8 | 1.05 KSA | 13.125 million | 196.875 million (93.75%) |
     | 4 | 50 000 000 – 62 499 999 | years 8–10 | 0.525 KSA | 6.5625 million | 203.4375 million (96.875%) |
     | 5 | 62 500 000 – 74 999 999 | years 10–12 | 0.2625 KSA | 3.28125 million | 206.71875 million (98.4375%) |
     | 6–29 | from 75 000 000 | years 12–59 | halving each period down to 1 base unit | 3.281249 million in total | 209 999 999 KSA (≈ 100%) |

     Halving is fast: 75% of the supply is issued in the first 4 years. About 17 280 slots per day: in the first halving period 145 152 KSA are issued per day. How each slot divides between the
     block share account and the producer, and the share's total, [not published, D-134].
- **Rationale**: the rounded figures use up the supply exactly and split every period without remainder; halving by DAA score rather than by the calendar is the approach already settled in D-074 (the DAA score is anchored to 5 seconds by ASERT and agrees with the
  calendar in the long run). Paying the share at unlock time follows the principle "no income until the data is on BTC" and does not break D-079's empty blocks writing no state. The ratio and the cutoff go into `Rules` because of "whether there is any afterwards to be discussed separately".
- **Impact**: a change of consensus parameters; implementing it means a new network (devnet / signet restart). **Implementation of the block share** (M16, 2026-10-04, the same PR as D-090 / D-091):
  - `ksana-primitives::params`: the base values `block_share_bps` / `block_share_end` in `Params`, fields of the same names in `Rules` and `Upgrade` (the upgrade schedule can change the ratio and
    the cutoff; `check_upgrades` rejects an upgrade whose ratio exceeds 10 000), `block_share_at(d)` (by the slot's own rules, `reward × bps / 10 000` rounded down) and
    `block_rewards_split_at(d, w)` (the (producer, share) pair for w slots, summing to `block_rewards_at`). devnet / signet values: 2 000 and 50 457 600
    (= 2 × devnet's halving interval); devnet's three issuance figures are still the placeholders of D-074 (the tests need a genesis allocation),
    and mainnet's 8.4 KSA / 12 500 000 / 2.1 × 10^16 and its block share go into the code together with the mainnet parameter set (§14). `max_genesis_supply` is unchanged.
  - Genesis config: `GenesisFile.block_share_account` (an optional x-only public key as 64 hex chars) → `GenesisConfig.block_share_account`, written to state key
    `0x0B` (§10.1, ABI §7); when omitted the key is not written and the block rewards go entirely to the producer. The field enters the Borsh encoding, so the fingerprints of all genesis configs change (it is a new network anyway).
    `check_supply` is unchanged. `devnet/init.sh` additionally generates `share.key` and writes it into the config.
  - Unlock in `ksana-execution`: read `0x0B` once; each rewarded position is split by `block_rewards_split_at`, `issue_reward` caps by the sum of the two parts,
    share = min(share portion, actually issued), producer = actually issued − share; the two accounts are credited separately (sponsored fees still go only to the producer). `UnlockItem` is unchanged; `determinism.rs`
    has no unlock entries, so the golden roots are unchanged.
  - Node RPC: the `locked_fees` of `ksana_getAccount` is estimated with the same split and cap, and when querying the share account it lists the portion each block will share with it (`docs/RPC.md`).
  - TS SDK: the two fields of `Params` / `Rules` / `Upgrade`, `blockShareAt`, `blockRewardsSplitAt`.
  - Tests: `params.rs` (per slot, across a halving point, the cutoff, saturation, the upgrade schedule changing the ratio, an upgrade schedule with a ratio above 100% is invalid), `rewards.rs` (the w slots of a blue block and of an empty block split slot by slot,
    none for a red block, sponsored fees not split, all to the producer after the cutoff, all to the producer when no account is configured, the share guaranteed first when capped), `m9_rewards` (the unlock in Side and the in-process node's
    `ksana_getAccount`), `params.test.ts`. The indexer and the node share the execution layer, so the state root check covers it naturally.
- **Status of the defaults**: (a) the block share account is confirmed [not published, D-134]; (b) the share applies only to block rewards, (c) no more share from `BLOCK_SHARE_END` on (changeable via the upgrade schedule),
  (d) mainnet has no genesis allocation, (e) the halving interval is 12 500 000 -- the project owner raised no objection; implemented per the defaults, with a separate decision to be recorded if they change.

## D-090 Empty headers keep going on-chain, empty-block difficulty increases with the number of consecutive empty blocks, no merged mining (decided by the project owner 2026-10-04; revises D-084 and SPEC §6.6, §6.7, §13; consensus change, M16)

- **Problem**: the two items still open after D-088: the concrete form of the fast-lane PoW (whether to reconsider merged mining because empty headers would go on-chain sparsely) and whether empty headers keep going on-chain. The design note handed to
  the project owner on 2026-10-03 proposed "form A": A1, empty headers are not settled on their own but deferred to the settlement of the next data block
  before going on-chain (local policy); A2, empty-block difficulty increases with the number of consecutive empty blocks up to a cap (consensus). The project owner, 2026-10-04 05:44 UTC: "let the empty blocks go on-chain; but the increasing empty-block difficulty must be done; no merged mining".
- **Decision**:
  1. Empty headers go on-chain with the settlement / republish payloads as before; A1 is not adopted: BTC-anchored finality (D-082) and the indexer that connects only to bitcoind stay as they are.
  2. Increasing empty-block difficulty (A2). Define the **empty-block streak** `streak(B)`: 0 for a non-empty block, `streak(SP(B)) + 1` for an empty block (propagated along the selected parent; the genesis block counts as 0; a block that is only merged
     and is not the selected parent does not affect the streak). The weight of an empty block is `w(B) = min(N × 2^(streak(B) − 1), N_MAX)`, N = `EMPTY_BLOCK_DIFFICULTY` (12, unchanged),
     N_MAX = `MAX_EMPTY_BLOCK_WEIGHT` (new parameter, 120); a non-empty block is still 1. The three uses of the weight are unchanged (D-084): effective target = the target of `bits` / w, DAA score increment w,
     w reward slots. Consecutive empty blocks thus have weights 12, 24, 48, 96, 120, 120, ..., and the idle header interval doubles block by block from about 60 seconds to about 10 minutes; when there are transactions the producer
     immediately switches to producing at weight 1 (the immediate recovery of D-087 is unchanged; `bits` does not vary with the streak), and the first empty block after that starts again from 12.
  3. `ASERT_HALF_LIFE` 600 seconds → 7 200 seconds: at the cap each empty block takes about 600 seconds, keeping the ratio of half-life to single-block time at 12 (the same order of magnitude as the 600 / 60 = 10 at the time of D-084),
     so the idle difficulty jitter is unchanged (the stationary standard deviation of the schedule error is about √(E·τ / 2 ln 2) ≈ 1 800 seconds, about ±19% in difficulty). The cost is a 12× slower response to hashrate changes: a 10× hashrate step
     converges in about 10 000 blocks (previously about 1 500); starting from the lowest difficulty, each doubling of difficulty takes about 1 440 blocks if all blocks are non-empty (previously 120).
  4. Both parameters are fixed per network and do not go into the upgrade schedule (D-088).
  5. No merged mining: D-087 stands. The concrete form of the fast-lane PoW is independent SHA256d on our own hashrate (GHOSTDAG) + BTC-anchored finality; the two rows "Concrete form of the fast-lane
     PoW" and "Whether empty headers keep going on-chain" in SPEC §17 are closed.
- **Rationale**:
  - Banning empty blocks cannot save cost: block rewards mean miners will always produce the cheapest valid blocks at the rate the difficulty allows, so banning empty blocks would only turn them into non-empty blocks with self-transfers, with about 70× more bytes on-chain instead
    (design note, section 2). The two-speed difficulty of D-084 is still the right lever; D-090 only lets the idle multiplier grow with the streak, cutting the headers of an idle day from about 1 440 (about 26 KB) to
    about 144 (about 2.6 KB).
  - By streak rather than by time: timestamps can be manipulated by the producer, whereas the streak count is a deterministic function of the DAG (the selected parent is decided by GHOSTDAG) that the node, the indexer and the header DAG can all compute from
    the headers, with no new consensus field; propagating only along the selected chain keeps the same basis of calculation as the DAA score and `bits`.
  - The cap of 120 (10 minutes): the same order of magnitude as the BTC block interval, so when idle there is still about one fast-lane header per BTC block and the finality determination (D-082) can still advance BTC block by BTC block; a higher cap
    would require a longer half-life (keeping the ratio of single-block time to half-life) and respond more slowly to hashrate changes.
  - Issuance per unit of time is unchanged (weight = reward slots), the streak changes nobody's expected income, and there is no incentive to spam empty blocks or deliberately break the streak; parallel empty blocks are each weighted by their own streak
    and are all rewarded, like the parallel empty blocks of D-084.
- **Impact**: consensus change, devnet / signet restart. `ksana-primitives::params`: `max_empty_block_weight`, `asert_half_life_secs` 7 200,
  `empty_block_weight(streak)`, `is_empty_block`, `block_streak`, `block_weight(header, sp_streak)`, `is_block_weight`. `ksana-consensus`:
  `Consensus::streak / weight(header, sp_streak)`, `check_header(sp, daa_score, weight, header)`, `work(header, weight)`,
  `solve(header, weight, …)`, `genesis_work`; `BlockDag` records each block's `streak`, with new `selected_parent` / `parent_streak` / `weight` / `streak`
  queries, and `insert` takes an extra `streak` parameter. `ksana-btc::headers` (header DAG), `ksana-node` (check / insert / commit / rebuild in `chain.rs`,
  `Template::tip_streak` and `empty_weight`, `Chain::weight_of` / `streak_of`, the producer's empty-block weight follows the streak at the main-chain tip), `ksana-execution`
  (the weight of an unlockable segment can only be 1 or the empty-block weight of some streak), TS SDK (only the half-life parameter changes; the SDK does not compute weights). Tests: consensus streak / cap / `bits` unchanged /
  convergence (with the enlarged half-life, the scenario starting from the lowest difficulty needs 65 000 blocks), streak propagation in GHOSTDAG, 24–120 slots and illegal weights in the execution layer, node and indexer tests now use the weights
  recorded in the DAG or derive the streak along a linear chain; `determinism.rs` has no unlock entries, so the golden roots are unchanged. devnet starts from the PoW limit: with only empty blocks the ramp-up is about 25 doublings ×
  12 blocks ≈ 300 blocks, comparable to before; with continuous transactions each doubling takes about 1 440 blocks, 12× slower than before, so soak tests and demos should set `bits` close to the local hashrate in the genesis config
  (`GenesisFile.bits`; CI's short soak test was already entirely within the ramp-up period, so its behavior is unchanged). The RPC and SDK interfaces are unchanged.
- **Follow-up**: the gap pointed out in section 3 of the design note (when a competing chain appears on BTC the node does not immediately republish its own heavier chain, so during an idle period a single normal-difficulty block that an attacker lands first may be judged
  final first) was decided by the project owner at 2026-10-04 07:01 UTC to be fixed; see D-091.

## D-091 Republish headers immediately when a lighter competing chain appears on BTC (decided by the project owner on 2026-10-04; local policy, M16; revises SPEC §7.6, §14, §15, §16, §17)

- **Problem**: an open item left by D-090. The headers of blocks that carry no settlement transaction are republished only in two cases (D-047, D-075): at least `MAX_SETTLEMENT_HEADERS − 1` blocks have followed it,
  or its anchor is more than `4 × SETTLEMENT_TIMEOUT` (24) blocks behind the BTC tip. When the on-chain part of this node's main chain is lighter than the other side's (typical case: there is no settlement transaction on chain yet,
  for example only empty blocks since the genesis block, or the producer never had a usable funding UTXO), an attacker settles a lighter chain onto BTC: the only blocks connected in the header DAG are the other side's, so it is the candidate of the finality
  determination (D-082); once its data has `BTC_FINALITY_CONFS` confirmations it is determined final, and this node's heavier chain is voided no matter how heavy it is (hours of empty blocks mined for nothing, the DAA score rolls back, the difficulty
  plunges), while this node waits for the two conditions above before republishing, which is too late. The project owner, 2026-10-04 07:01 UTC: "do it".
- **Decision**: republishing gets one more trigger condition (`Chain::btc_competitor`, `rescue.rs`). After each round of BTC sync, if the tips of the header DAG (the block headers that have appeared in BTC payloads) include
  a **block of a competing chain** -- one that first appeared on the current BTC best chain at least one and fewer than `BTC_FINALITY_CONFS` BTC blocks ago, is not on this node's main chain, and whose sort key `(blue_work, hash)`
  is lower than this node's main-chain tip yet higher than the highest connected block on this node's main chain (its own header and the headers of its entire past have landed on BTC) -- it republishes immediately, without waiting for the two conditions above or for the depth to reach
  `SETTLEMENT_DEPTH`: the consecutive, not-yet-landed headers of blocks without settlement transactions, from the first block whose data has not landed up to the main-chain tip (with the bodies of those among them that need a body); the same segment
  (identified by its starting hash) is not resent before it confirms or is evicted. Once this node's chain is connected on BTC (the sort key of the connected part is not lower than the other side's), the other side's blocks no longer count as a competing chain and republishing returns to its original
  cadence. One log line is written per competing-chain block.
- **Rationale**:
  - The criterion matches the finality determination: the candidate is the block with the largest sort key among the connected blocks whose selected chain passes through the finalized tip. When the sort key of the other side's block is not higher than this node's connected part, it cannot become the candidate and there is no need
    to republish; when it is not lower than this node's main-chain tip, this node switches to it (or is already requesting it from peers, D-082), and again there is no need to republish. Only blocks that appeared within the last `BTC_FINALITY_CONFS` BTC blocks
    are considered: earlier ones are either already finalized (this node has switched to them) or already overtaken by a heavier connected chain, and republishing would change nothing.
  - Not waiting for the depth to reach `SETTLEMENT_DEPTH`: what the other side is racing for is finality, and this node must get a segment of chain heavier than the other side's connected on BTC before the other side's data reaches the confirmation count; if it sent only the part of sufficient depth, this node's
    chain might still be lighter than the other side's for a while, and it would take two more blocks (about 20 minutes at the cap) to catch up. Even if blocks near the tip are reorged out later, having their headers on chain is harmless: in the header DAG such a block is just a
    side-chain block, and the candidate is still the heavier chain; the extra cost is only a dozen or so bytes per header.
  - Waiting one BTC block after it appears: with multiple producers it is normal for a side-chain block that is parallel to this node's blocks and merged by this node's main chain to land first (its past does not contain this node's parallel blocks, so it does not carry them),
    and this node's next block with a settlement transaction usually carries its own headers right away (settlement batching waits for the in-flight settlement to confirm before carrying, D-077), so republishing as soon as it appears wastes money; republishing only when there is still no carrier after one BTC block
    (typically the chain is idle, the unlock cursor is stuck on a block that has not landed, and no block needs a body) still leaves a margin of 3 BTC blocks before the other side reaches the confirmation count.
    The "a parallel carrier landing first does not trigger a republish" scenario in `m12_batch.rs` is therefore unchanged.
  - Still a local policy: consensus, the finality rules and the indexer are unchanged; several nodes may each send one, and duplicates are ignored by the indexer (as in D-047).
- **Impact**: `ksana-node`: `Chain::btc_competitor(snapshot, within)` and `Chain::COMPETITOR_GRACE` (1), the trigger and range in `rescue.rs`, `Rescuer`
  remembers the competing-chain blocks already logged; the test `a_lighter_chain_on_btc_triggers_an_immediate_republish` in `m15_finality.rs` (A produces only empty blocks from the genesis block, B settles a
  lighter chain with transfers onto BTC: no republish while it has been visible for less than one BTC block, after one BTC block A republishes 7 headers, does not resend them, and no longer triggers once they have landed; when B's blocks reach the confirmation count
  they are not finalized and A's chain is; B switches over after obtaining A's chain), and the parallel-carrier scenario in `m12_batch.rs` is unchanged. SPEC §7.6 gains the trigger condition,
  §15 and §16 each gain one line, and the open items in §14 and §17 are closed. RPC, SDK and indexer are unchanged.
- **Revised by D-104** (M29): a tip whose selected chain holds a block anchored, or merging a block anchored, off the BTC best chain, or one the finality determination excluded, counts only below
  the lowest such block: after a deep BTC reorg the chain the nodes left lands again on the new branch, and nothing above that block can become final.

## D-092 The account is the Taproot address (decided by the project owner on 2026-10-04; revises SPEC §1, §4, §4.2, §11.3, §11.4, §16, §17 and D-052; consensus change, M17)

- **Problem**: previously the account was the x-only **internal** key P of the user's BTC key (SPEC v2 §4), while the wallet address `bc1p…` holds its BIP86-tweaked **output** key
  Q = P + H_TapTweak(P)·G: the address can be computed from the account, but the account cannot be computed back from the address; a P2WPKH account (D-052) is the x coordinate of the compressed public key, which has even less of a reversible relation to the address (a public key hash).
  So the wallet page has to show two different strings, the "address" and the "Ksana account": to receive KSA you give the other party 64 hex chars, to receive BTC you give the address. KSA transferred to the bytes of a mistakenly pasted `bc1p`
  address is credited to the output key, and only someone holding the tweaked private key can move it (in practice a loss); KSA transferred to the bytes of a `bc1q` address is lost outright. `builtin::transfer`
  does not check whether `to` is on the curve, nor should consensus (a contract ID is 32 bytes too). The project owner, 2026-10-04 10:13 UTC: "this makes mistaken payments easy... can these two addresses be unified"; at 10:16 UTC chose "unify on the Taproot address" on the decision card.
- **Decision**: account := the 32-byte output key Q of the user's Taproot address (BIP86 key-path, no script tree). The address is just the bech32m spelling of the same bytes (devnet / regtest
  `bcrt1p…`, signet `tb1p…`, mainnet `bc1p…`); everywhere an account is accepted (CLI, SDK, the node's `--producer`, the demo page) accepts either the address or 64 hex chars.
  Signing: the verification key of `Bip340` is still `account` itself, so local signers sign with the BIP86-tweaked private key (`ksana_crypto::tweaked_keypair`, the SDK's
  `taprootTweakSecretKey`); the `message_challenge` of `Bip322Simple` is the account address `OP_1 <account>`, the witness accepts only a single P2TR key-path signature,
  and the P2WPKH accounts of D-052 are dropped (revises D-052; `verify_simple` can still verify signatures of P2WPKH addresses, used only for the official BIP322 vectors). A contract ID still has only the 64-hex-char
  form and no address (otherwise someone would send BTC to a contract "address"); 32 bytes off the curve cannot be turned into an address, and clients refuse to parse a non-Taproot address as an account.
- **Rationale**: one key, one identifier: to receive BTC and to receive KSA you give the other party the same `bc1p` address, a wrong paste still lands on the same key, and account and address are mutually reversible; browser wallets (Unisat,
  Xverse's ordinals address) already use BIP86 Taproot addresses, BIP322 signing uses this same address, and verification no longer needs the "account → tweak → address" step; the producer's funding
  address (`producer_script`, BIP86) and its account also become the same bytes. Giving up P2WPKH accounts is the price: they have no reversible address ↔ public key relation, so unification is impossible with them; users who need
  to simply switch to a Taproot address in their wallet. The alternative (keep the internal key and only add a checksummed `ksa1…` account format, no consensus change) would prevent wrong pastes but would still be two strings; not chosen.
- **Impact**: consensus change (the account of a given private key is no longer the previous value), devnet / signet restart; the shared vectors are regenerated (in `signing.json` the accounts, signatures and `bip322`
  cases are P2TR only; `codec.json` / `merkle.json` are rewritten along with the generator), and the golden roots in `ksana-execution/tests/determinism.rs` are updated. `ksana-crypto`:
  `account_of` returns the tweaked output key, new `tweaked_keypair`, `account_script`, new module `account.rs` (`btc_network`, `account_address`,
  `account_from_address`, `parse_account`), `sign_*_p2wpkh` removed, `SigError::InvalidAddress`. `ksana-cli`: account arguments accept addresses,
  `--sig bip340|bip322`, `key` prints account / address / script / internal key, the `account` query includes `address`, `--network` defaults according to `--chain`.
  `ksana-keytool` prints `account` (= the output key of the funding address) and `internal_key` (no more `pubkey`), `devnet/init.sh` now reads `.account`,
  `--producer` accepts an address, `soak.sh` has only two signing methods left. TS SDK: `accountOf` = the tweaked output key, `signVmop` uses the tweaked private key,
  `signBip322Simple` / `signVmopBip322` no longer take an address-type parameter (`signBip322SimpleP2tr` is kept as an alias), `WalletAccount` gains `internalKey`,
  `deriveWalletAccount` accepts only Taproot addresses, new `accountToAddress` / `accountFromAddress` / `parseAccount` / `btcNetworkOf`, the mock wallet defaults to
  Taproot (`p2wpkh` is only used to simulate a wallet that cannot serve as an account), the demo page's payee account accepts an address and shows that address and account are the same bytes. Both SDK wallets in the integration test `m6` are
  Taproot, and the seller's P2WPKH receiving script in `m13` is unchanged (the receiving script may be any standard script). The RPC JSON is unchanged (accounts are still 64 hex chars). The web wallet / block explorer
  branch (`claude/brave-carson-haoi9i`, based on m7) has not yet been updated for this decision and must be changed before merging.

## D-093 Cleanup of implementation limitations: deep switches and crash recovery rebuild the state from the genesis block, the node rebuilds missing blocks from the data on BTC (2026-10-04; revises SPEC §6.4, §6.5, §10.2, §10.3, §16; local implementation, no consensus change, M18)

- **Problem**: the remaining-issues list (section 2 of `REMAINING-ISSUES-2026-10-03.md`) lists 10 implementation limitations; on 2026-10-04 the project owner decided "let's do everything that should be done". Checking them one by one against
  `main` (after M17):
  1. The forced switch to a BTC-final chain (D-082) is bound by `REORG_WINDOW`: when the fork point is deeper than it, the node only alerts and operators have to wipe the data directory and resync; the indexer can already rebuild from genesis in this case.
  2. Blocks the finality determination is waiting for can only be requested from peers: when all peers are on another chain, or there are no peers, the node stops there, while the indexer can rebuild all block bodies from BTC alone.
  3. The header DAG, the finality state and the GHOSTDAG data all live in memory and are not pruned -- a mainnet-scale storage design (SPEC §17 "pruning of GHOSTDAG data"), not a local patch that can be done this time.
  4. "The D-083 bound is not checked when the snapshot is behind" -- verified to have no gap: the snapshot's base is not higher than the genesis anchor, `check_anchor` rejects blocks whose anchor is above the snapshot's tip for a retryable local reason,
     whenever any block is received the anchor block's time is in the snapshot; and the anchor of a header in a payload is necessarily below the scanned height.
  5. The overlap of D-084 reward slots and unlock entries, 6. D-085 exchange details, 7. D-086 path C restrictions, 8. the entry environment of merged blocks, 9. the indexer stopping on an invalid block -- all are behavior spelled out in the SPEC
     or deliberate conservative trade-offs; changing them needs a decision by the project owner and is out of scope this time (suggestions are listed in the thread reply).
  10. The test utility `calibrated_bits` is unused.
- **Decision** (items 1, 2 and 10):
  - **Rebuild the state from genesis** (item 1): `StateDb::reset` clears the state database; the node's forced switch to a finalized chain is no longer bound by `REORG_WINDOW` -- when the state at the fork point has been pruned, it logs
    an alert at the highest level, clears the state database, rebuilds the genesis state, re-executes along the old main chain from the genesis block to the fork point (`Chain::rewind_deep`), then attaches the new chain; no new blocks are processed in the meantime. Ordinary work-based
    switches remain bound by `REORG_WINDOW` (SPEC §6.4 unchanged). Crash recovery rebuilds the same way when it finds no state version consistent with the main chain (state database lost, belonging to another chain, or corrupted):
    it rewinds the main chain to the genesis block and, after BTC sync, `resume` re-executes the whole old main chain; previously this case simply exited with an error.
  - **Rebuild missing blocks from the data on BTC** (item 2): the indexer's logic for determining block bodies (candidate bodies, derivation and conversion of forced-inclusion entries, anchor-invalid blocks) moves from `ksana-indexer`
    to `ksana-btc::bodies`, shared by node and indexer. While scanning BTC payloads the node collects candidate bodies for blocks it does not have yet; at the end of each round of BTC sync
    (`Chain::rebuild_missing_from_btc`), for the blocks the finality determination is waiting for and the missing parents of orphans, if the header is already in the header DAG and the body can be determined, it assembles a block without settlement transactions
    (D-077: settlement transactions are not part of a block's identity) and receives it under all the rules for receiving historical blocks; when the parent is missing too it first becomes an orphan and the parent is rebuilt in the next round; once a block is obtained its candidates are dropped.
    Requests to peers continue as usual; whichever arrives first is fine.
  - **Data landing status of missing blocks** (a gap found by the adversarial-testing thread on 2026-10-04, also part of item 2): previously the node answered "undetermined" for every block it did not have, and the finality determination
    paused there waiting for the block -- an attacker only has to put the header of a block that needs a body onto BTC and withhold the body, and the node stays at its height forever, while the indexer, according to the data on BTC, answers
    "not yet landed" and proceeds as usual, so the finalized tips of the two diverge. Now the node answers for blocks it does not have by the same criterion as the indexer (`ksana_btc::bodies::Bodies::carries` /
    `body`): when the header is in the header DAG, its anchor is on the current best chain and its parents are known, whether it needs a body can be determined; when it does and no payload on the current best chain carries the same body,
    it answers "not yet landed" and the determination does not pause; in the other cases (no body needed, a payload carries a matching body, or whether a body is needed cannot be determined yet) it still answers "undetermined",
    and the node first rebuilds the block (within the same round of BTC sync) or waits for a peer to provide it, then answers from its own entries -- the node never determines a block it does not yet have to be final, and the determination result is the same as the indexer's.
    This is a node-side implementation fix for §6.5 item 3, "node and indexer determine with the same function"; no consensus change.
  - Remove `calibrated_bits` (item 10).
- **Rationale**: both items give the node capabilities the indexer already has: the indexer can rebuild all block bodies from BTC alone and rebuild the state from genesis, so there is no reason for the node to stop in front of the same data or to require a manual wipe of
  the data directory. Rebuilding from genesis is more reliable than keeping more state versions: the depth of a switch after a BTC reorg revokes finality has no upper bound, so no retention count is enough; a rebuild happens only on a forced switch whose fork point has been pruned,
  and is never triggered in normal operation. Rebuilt blocks go through the same validation path as blocks provided by peers (header, anchor, timestamp bound, entry rules, forced inclusion, execution and `state_root`), so no
  new trust is introduced: the data on BTC is just another source. Rebuilt blocks carry no settlement transactions; `Block.settlement` never took part in the hash anyway, and the node stores the copy it saw itself.
- **Impact**: `ksana-state`: `StateDb::reset`. `ksana-btc`: new module `bodies.rs` (`Bodies::new / observe / body / carries / data_status / known /
  forget`, `forced`, `parent_anchor`), `ksana-indexer::known` now uses it (behavior unchanged). `ksana-node`: `Chain::rewind_deep`, `rewind_store`,
  `reset_state_to_genesis`, the rebuild branch of `recover`, `update_virtual` lifting `REORG_WINDOW` for forced switches, `FinalityState` carrying candidate bodies,
  `KnownBlocks::observe_payload` collecting candidates, `Chain::rebuild_missing_from_btc` (called by `sync_btc`), `Chain::data_status` answering for missing blocks according to
  `Bodies`. Behavior changes: while syncing BTC the node now obtains on its own the blocks that are final on BTC but missing locally; the two "switch after obtaining the other side's chain" scenarios in `m15_finality.rs` become
  "rebuild and switch while syncing BTC"; a block whose header is on chain but whose body is withheld no longer stalls the node's finality determination. New tests `m18_rebuild.rs` (a deep forced switch rebuilds the state and
  keeps extending as usual, and restarts; an ordinary deep switch is still rejected; recovery when the state database belongs to another chain or is lost), `m18_btc_rebuild.rs` (a node without peers rebuilds blocks from settlement payloads and
  keeps up with finality; the header-only empty blocks republished per D-091 are likewise rebuilt and switched to; a header whose body is withheld neither stalls the determination nor gets requested, and the body landing later does not change the finalized chain),
  indexer tests (in the same scenario the finalized tips and states of node and indexer agree), and the `reset` unit test of `ksana-state`. SPEC §6.4, §6.5 (items 3 and 5), §10.2,
  §10.3 and §16 revised; the behavior of consensus, RPC, SDK and indexer is unchanged.

## D-094 Mining algorithm changed to the CPU-friendly Argon2id (2026-10-05, decided by the project owner; revises SPEC §2, §6, §6.6, §12, §13, §15, §16, §17; consensus change, M22)

- **Problem**: after D-087 dropped merged mining, SHA256d as the algorithm of an independent chain has only drawbacks left: BTC ASIC hashrate can be rented at any time to overwhelm the fast lane (the 51% line in §16),
  and ordinary users have no chance of taking part in block production. The project owner, 2026-10-05 07:29 UTC: "I want to change the mining algorithm to a CPU-friendly one; since sha256 cannot be merge-mined, let's just do a CPU algorithm directly". Candidates were RandomX (Monero's algorithm) and Argon2id (RFC 9106): the thread first evaluated and implemented RandomX, the owner chose RandomX at 07:38 UTC,
  asked at 07:39 UTC whether a browser could verify / mine, and at 07:42 UTC settled on "then let's use argon2id" -- the motivation being browser verifiability, and perhaps browser mining later. The RandomX code
  does not go into the repository.
- **Evaluation** (4 vCPU cloud VM, release build, single thread):
  - RandomX (randomx-rs 1.6.0 / the official C++ library, matches the official vectors): light mode 256 MB, cache initialization 0.6–1.7 s, 31–33 ms per hash; fast mode 2 GB,
    dataset initialization 66–69 s, 5.4 ms per hash. Verifying one header takes 33 ms (light mode) or keeps 2 GB resident (fast mode); it needs a C++ toolchain (cmake) and
    `-march=native`, the Rust bindings are not Send / Sync (the FFI would have to be written by hand); in the browser there is only a theoretical wasm port, at about 1/100 of native speed. At 17 280 headers per day for a busy chain
    (one block every 5 s), syncing one year means verifying 6.3 million headers: about 58 hours in light mode.
  - Argon2id (RustCrypto `argon2` 0.6.0, pure Rust, selects AVX2 at runtime on x86_64, takes the scalar path on aarch64, same results): with 1 / 2 / 4 / 16 / 64 MiB of memory
    one hash takes 0.47 / 0.94 / 2.0 / 10 / 74 ms; at 2 MiB and 1 pass, verifying a header takes 0.98 ms (1 024 per second) and one solve attempt (reusing memory) 0.86 ms.
    Syncing one year of a busy chain takes about 1.7 hours single-threaded. Browser: the pure-JS implementation in `@noble/hashes` verifies one header in about 13 ms (measured on Node 22; browsers are in the same order of magnitude).
  - For comparison: the block hash `sha256d` takes about 1.6 µs each.
- **Decision**:
  - The mining algorithm becomes Argon2id (RFC 9106, version 0x13):
    `pow_value(header) = LE(Argon2id(P = borsh(header), S = "ksana-pow" ‖ CHAIN_ID as 4 little-endian bytes, m = POW_ARGON2_MEMORY, t = POW_ARGON2_ITERATIONS, p = POW_ARGON2_LANES, no secret key K or associated data X, output T = 32 bytes))`.
    The message is the complete header encoding (the nonce is the last 8 bytes); the salt contains the chain ID (headers of different networks already differ naturally because the genesis blocks differ; this is only domain separation, and it also lets the devnet and
    signet vectors be told apart); the output is interpreted as little-endian, consistent with the block hash convention of D-025.
  - Parameters (devnet and signet, SPEC §13): 2 048 KiB of memory, 1 pass, 1 lane, 32-byte output. Trade-off: verifying one header takes about 1 ms -- about
    1 000 times slower than SHA256d, but the header DAG, the finality determination and the indexer compute each header only once, and syncing one year of a busy chain still stays under 2 hours (header checks can run in parallel); 2 MiB keeps
    each parallel verification at only 2 MiB, and browsers can compute it too; with more memory (16 MiB and up) the sync cost multiplies while the relative advantage of GPUs does not drop noticeably with it.
    The parameters are fixed per network and live in `Params` (`PowAlgorithmId::Argon2id(Argon2Params)`), not in the upgrade schedule: the path to changing them is to add them to `Upgrade` / `Rules`
    (D-076) and switch by DAA score, or a new network; not done in this phase.
  - Unchanged: the block hash (block identifier) `sha256d(borsh(header))`, the header format and `nonce: u64`, `bits` / target / work / ASERT (D-025, D-026),
    two-speed difficulty and weight (D-084, D-090), finality (D-082), republishing (D-091), the economic parameters (D-089), the upgrade mechanism (D-076). Only the value in "PoW value ≤ effective target"
    is computed by a different algorithm.
  - An honest caveat: Argon2id is not "CPU-only mining". Argon2id at 2 MiB and 1 pass is memory-bandwidth bound: GPU memory bandwidth is 10–30 times the memory bandwidth of an ordinary CPU,
    and the hashrate-per-unit-cost advantage of GPUs is estimated to be of the same order (several orders of magnitude smaller than the ASIC advantage on SHA-256, but not zero); the FPGA / ASIC advantage depends on on-chip memory, and 2 MiB
    fits in on-chip SRAM. Per §16, the fast lane's security goal is the cost of "rolling back blocks that are not yet BTC-final", not absolute ASIC resistance; the purpose of choosing Argon2id is to let ordinary computers and
    browsers take part and verify, and its implementation is simple with good determinism (pure integer arithmetic, identical results on x86_64 and aarch64).
  - Retuning path: before mainnet, tune `POW_ARGON2_MEMORY` / `POW_ARGON2_ITERATIONS` by the measured GPU advantage and sync cost (mainnet parameters, §17); a running network switches via the upgrade schedule
    (which first requires adding the parameters to `Rules`).
  - Browser mining is out of scope for this phase: the SDK only provides verification (`checkPow`); nothing in the design prevents adding a mining page later (the same `argon2id` function suffices to solve).
- **Implementation** (M22):
  - `ksana-primitives`: `PowAlgorithmId::Argon2id(Argon2Params { memory_kib, iterations, lanes })`, `Argon2Params::salt(chain_id)`,
    `OUTPUT_LEN`, `SALT_PREFIX`; devnet / signet use 2048 / 1 / 1.
  - `ksana-consensus`: `Argon2id` (`pow_hash` / `pow_value` / memory-reusing `solve`; parameters outside Argon2's range give `InvalidParams`),
    `PowAlgorithm::pow_value` / `solve` now return `Result`: a computation failure (memory allocation failure) is the new error `ConsensusError::PowUnavailable`, a local fault,
    which the node maps to `NodeError::Internal` without deeming the block invalid; `PowAlgorithm::solve_chunk` / `Consensus::solve_chunk` (the number of nonces per solve attempt,
    16 for Argon2id, about 15 ms; the block production loop checks between attempts whether the template has expired); `pow_algorithm(&Params)`. The SHA256d midstate solver
    (`miner.rs`, `solve_header`) is removed; `Sha256d` is kept as the algorithm for tests and benchmarks (the algorithm-independent rule tests of the consensus layer, and the node tests and regtest integration tests that have to produce hundreds of empty blocks in a row use it through the D-062 injection point (indexer and node with the same algorithm); it is about
    1 000 times faster than Argon2id: once the empty-block streak weight reaches 120, Argon2id takes about 0.25 s per block; the remaining node and indexer tests produce and verify blocks with the default network algorithm).
  - `ksana-node`: the block production loop solves in batches of `solve_chunk`; tests set the genesis difficulty from the measured Argon2id speed (`pow_rate`). The header DAG
    (`ksana_btc::headers`) returns the new `Insert::Unavailable` for a header it cannot check: the node fails the current round of BTC sync, the indexer stops the current round of processing, both alert,
    and the header is not treated as invalid (shared by node and indexer).
  - Vectors and tests: `tests/vectors/pow.json` (`cargo run -p ksana-consensus --example gen_pow_vectors`, 10 cases: the genesis block, a non-empty block solved at the minimum difficulty
    and an empty block of weight 12, the same empty block checked at weight 24, three parents and the maximum nonce, a target below the limit, the signet salt, parameters of 64 KiB / 2 passes / 2 lanes and
    8 KiB / 3 passes), checked case by case by Rust `tests/argon2id.rs` and TS `test/pow.test.ts`; the `argon2` crate is checked against the Argon2id vector of RFC 9106 §5.3;
    the aarch64 job in CI additionally runs `ksana-consensus`.
  - TS SDK: `pow.ts` (`decodeCompact` / `targetFromBits` / `effectiveTarget` / `powSalt` / `powHash` / `powValue` / `checkPow`, verification only),
    `Params.pow_algorithm`; uses `argon2id` from the existing dependency `@noble/hashes`. The web wallet / block explorer do not verify headers for now (later work).
  - New dependency: `argon2` 0.6.0 (RustCrypto, pure Rust, `default-features = false, features = ["alloc"]`; brings in `blake2` 0.11 and `base64ct`).
    Like wasmtime and jmt it is a consensus parameter: upgrading its version requires checking the vectors. In debug builds `argon2` / `blake2` are compiled with `opt-level = 3` (only affects test duration).
  - Impact: consensus change, devnet / signet restart (the genesis block format is unchanged, blocks after it use the new algorithm). RPC, the execution layer, settlement, the P2P protocol and the block format are unchanged;
    the `devnet/` images need no change (pure Rust, no cmake needed). The shape of the node's CPU usage for block production changes: each attempt takes about 1 ms, and when idle the number of attempts
    for producing at the empty-block weight is unchanged.

## D-095 Carrier reward: the producer who lands a block's data on chain gets 5% of each of that block's reward slots and half of the path B sponsored fees in it (decided by the project owner 2026-10-05; revises D-077, D-089 and SPEC §7.1, §7.5, §7.6, §8, §8.1, §8.5, §8.6, §13, §16, §17; consensus change, implemented in M23)

- **Problem**: since D-077, the body of any block can be carried on chain by any later payload, and the carrier pays the BTC fee without any direct income (SPEC §7.5 "Cost", §17
  "Settlement funding and carrier reward"). The thread "Fee competition brainstorm" (the project owner, 2026-10-05 10:21 UTC: "how do we make sure that producers compete when packing transactions, or in other words that the optimal fee can be offered?") checked the current state against the code (`ksana-mempool`, `producer.rs`, `executor.rs`, `tracker.rs`): a path B sponsored fee is an
  absolute amount that goes entirely to `header.producer`; producers pack greedily by absolute amount; a producer without a configured funding wallet packs no path B entry at all (local policy); the weighted
  reward slots (D-084, D-090) make the expected reward per hash independent of whether anything is packed; a block's BTC bytes are paid by the producer of the next batched settlement, not necessarily by whoever packed it. Conclusion:
  packing is a cost center (the sponsored fees are negligible next to the producer's part of the 8.4 KSA reward per slot, and the BTC byte cost lands on someone else), producers do not compete over packing, and users have no "optimal
  fee" to speak of; after D-094 producers are mostly CPU miners without BTC, who will all the more produce only empty blocks.
- **Discussion**: the project owner first proposed "the ideal approach is that whoever packs is the one who submits to BTC"; the thread pointed out three costs (every packer must have BTC; giving up D-077's batching means, at full load,
  about 0.4 BTC more per day; a fallback carrier is still needed), and the owner agreed at 11:18 UTC: "this design won't work". 11:21 UTC: "the people who submit the BTC transactions have to profit from it; only then will everyone submit transactions on their own initiative" set the direction: pay the carrier. 13:03 UTC: "let each block accumulate some KSA for whoever submits the transaction. A 5% share?" and asked whether private silent packing is supported;
  the thread set out the scheme below, and at 13:17 UTC: "OK".
- **Decision** (consensus rules):
  1. **Carrier**: the carrier of block B, `carrier(B)`, is the sender of the payload that first landed B's data on chain. "First landed on chain" is exactly `data(B)` of §6.4: a block that needs a body
     takes the first payload on the current BTC best chain that carried its header together with the same body; an empty block or a block containing only forced-inclusion entries takes the payload in which its header first appeared; when there are several at the same BTC
     height, the one whose transaction comes earliest in the block is taken (the existing implementation, `settlement_of` in `tracker.rs`, already does this; this entry writes it down as a consensus rule). Later duplicate payloads get
     nothing and merely pay BTC fees for nothing. This entry does not change the definition of `data(B)`.
     - When that payload is a settlement payload (§7.1 `Settlement`, whose last header is the block C that carries this settlement transaction), `carrier(B) = C.producer`; when B carries the settlement transaction itself,
       `carrier(B) = B.producer`. C is required to have been accepted by the main chain -- a block on the main chain of the selected parent of the block that unlocks B, or a block merged by it (C need not be on the main chain, but must be in
       that block's past); otherwise B has no carrier. The format of the settlement payload is unchanged; to collect the carrier reward one must first mine a block. (The first draft of this decision said "C's header passes the header
       DAG check"; the implementation changed it to "accepted by the main chain": the outcome of the header check depends on when each node saw that header and is not deterministic, whereas the main chain, when the unlocking block is executed,
       is the same for nodes and the indexer. C's settlement transaction takes a few minutes to confirm while C itself is accepted within seconds, so the two formulations give the same result under normal conditions.)
     - When that payload is a republish payload (`Headers`), the payload gains a new field `payee` (32-byte payee account); the payee is eligible only when B's data has waited more than `CARRIER_REPOST_DELAY` (2)
       BTC blocks without landing on chain: `data(B) − B.btc_anchor.height > ANCHOR_DEPTH + CARRIER_REPOST_DELAY` (at block production the anchor is usually `ANCHOR_DEPTH` blocks
       below the tip, so this means "the data landed on chain only after waiting 3 or more BTC blocks after the block was produced"; measured the same way as the in-flight rule of §7.5 measures "the anchor behind the tip").
       When this is not met, B has no carrier. Within the normal window, therefore, only someone who mined a block can collect, and people without hashrate cannot grab the reward by racing to send BTC transactions. An all-zero `payee` means no reward is claimed (B has no
       carrier; the reward goes to the producer).
     - A rescue settlement (§7.3) is a copy of the original payload, whose last header is still the original block, so the attribution is unchanged and the rescuer, as today, has no direct income (it may instead send a republish payload carrying a `payee`;
       local policy, to be discussed separately).
     - BTC reorgs are handled by the anchored view (§6.8); rewards already paid out are not reverted.
  2. **Fixed part**: each reward slot d of B (§8.6) gives `reward(d) × CARRIER_BPS / 10 000` (`CARRIER_BPS` = 500, i.e. 5%, rounded down) to `carrier(B)`,
     taken out of the producer's portion, with the block-share portion unchanged: in the first halving period each slot of 8.4 KSA pays the carrier 0.42 KSA, and after the share period ends carrier 5% /
     producer 95%. It is counted per slot, so the carrier reward per unit of time is constant: about 0.42 KSA every 5 seconds, about 50 KSA per BTC block, independent of block size, meant to cover the fixed overhead of a settlement transaction
     that does not depend on the block (commit transaction, envelope, headers; at 10 sat/vB each settlement is about 250–750 vB, so a KSA worth 0.05–0.15 USD breaks even). Blocks without a reward
     (red blocks) have no fixed part; when B has no carrier (the two ineligible cases of item 1) this portion is not deducted and stays with the producer.
     `BLOCK_SHARE_BPS + CARRIER_BPS ≤ 10 000`, checked with the upgrade schedule.
  3. **Proportional part**: of the sponsored fee `amount` of each path B entry executed in B, `amount × CARRIER_FEE_BPS / 10 000` (`CARRIER_FEE_BPS` = 5 000, i.e. half,
     rounded down) goes to `carrier(B)` and the rest to `B.producer`. Skipped entries (skipped by nonce when merged, §6.3) have no fee and no reward; their bytes are covered by the fixed
     part. Without a carrier, everything goes to the producer.
  4. **Payout timing**: both at unlock (§8.5). When the unlock entry processes B's position, the execution layer asks the anchored view for `carrier(B)` (`BtcView::carrier`; nodes and the indexer
     answer from the payload senders recorded in the BTC snapshot and the main chain at execution time; by then B has entered the settled closure and `data(B)` is known), splits the reward into three portions, share, carrier and producer,
     splits the sponsored-fee record at that position into two portions, carrier and producer, and credits each to the available balance; when the max supply cap binds, the share is guaranteed first, then the carrier, and the producer takes what is left. When the carrier is the producer
     itself, it is the same as having no carrier (everything goes to the producer). The locked record `LockedFee { producer, amount }` and the state key (§10.1) are unchanged; the split is done at unlock.
     `locked_fees` in `ksana_getAccount` also lists, for a carrier, the reward it will receive.
  5. **Parameters**: `CARRIER_BPS` 500 and `CARRIER_FEE_BPS` 5 000 are rules that vary with the protocol version (`Rules` / `Upgrade`, §6.9), with the rules taken by the DAA score of the unlocked slot (fixed part)
     or block (proportional part) itself, adjustable through the upgrade schedule; `CARRIER_REPOST_DELAY` of 2 BTC blocks is fixed per network (`Params`). devnet, signet and
     mainnet use the same values.
  6. **Unchanged**: no minimum fee rate is added and no fees are burned (the market pricing of §5.1 and §8.2 and item 3 of D-087, "one's own transactions are transactions too", stand); D-077's batching policy, D-079's
     empty blocks writing no state, the weights and reward slots of D-084 / D-090, the total supply and block share account of D-089, D-082 finality, and the settlement transaction structure of §7.2.
  7. **Private silent packing** (the project owner asked "do you support private silent packing?"): neither meaning needs a protocol rule. A producer without BTC that only packs, does not land its data on chain itself, and lets someone else
     carry it: supported, this entry is designed exactly for it -- the packer gets half of the sponsored fees and 95% of the reward, the carrier gets 5% and the other half; the local policy "a producer without a configured funding wallet does not pack path B"
     (§7.5) is dropped with M23. A user who does not broadcast but hands the op privately to one particular producer: the protocol cannot stop this and should not (it is a common anti-front-running practice); it is not made into
     a protocol or RPC feature, and the wallet and CLI broadcast by default; the cost, that only that one producer can include it and the wait is long, is borne by the user.
- **Rationale**:
  - The fixed part is per slot rather than per byte: the overhead of the headers and the envelope does not depend on block size; counting per slot takes the same code path as the block share, is the simplest to implement, and keeps the total per unit of time constant, neither amplified nor shrunk by empty-block
    weights. The cost is that the 5% of a single high-weight empty block far exceeds its 18-byte cost (50.4 KSA at weight 120), while a weight-1 block with a full body gets only 0.42 KSA --
    the proportional part makes up for it.
  - Sponsored fees split in half: at full load the carrier pays about 0.3 BTC per day (10 sat/vB) and the 5% pool is about 7 thousand KSA per day, so below 4 USD per KSA it runs at a loss; without this half, all sponsored fees go into
    the packer's pocket while the packer may have no BTC. Half is the starting point; the ratio goes into the rules table and can be adjusted after observing signet.
  - Paying the producer of the block the settlement payload belongs to rather than writing a payee into the settlement payload: the format is unchanged and no new signature or binding is needed; "you must mine a block to collect" keeps people without hashrate from
    grabbing the reward by racing to send BTC transactions, and the 2-block wait for republishing serves the same purpose.
  - Without a carrier it goes to the producer: this portion is taken out of the producer's portion to pay the carrier in the first place; when nobody does that work, it is not deducted.
  - The user's optimal fee (the thread's analysis): the lower bound is the op's BTC byte cost (a transfer is 21–36 vB after compression), the upper bound is the cost of path C (330–400 vB plus about
    50 minutes); once the carrier has income, the byte cost has a payer, and competition among packers pushes the fee toward the lower bound.
- **Impact**: consensus change (the crediting rules at unlock, the republish payload format, new rules parameters), devnet / signet restart; the golden roots of the determinism-test scenarios that contain unlock entries
  are regenerated. **Implementation** (M23, 2026-10-05, the project owner chose to start immediately):
  - `ksana-primitives`: `carrier_bps` and `carrier_fee_bps` in `Params` / `Rules` / `Upgrade` and `Params::carrier_repost_delay` (devnet and
    signet 500 / 5 000 / 2); `block_rewards_split_at` changed to a three-way `RewardSplit { producer, share, carrier }`, `block_carrier_at` (each slot by its own
    rules, not exceeding what remains after deducting the share) and `carrier_fee_at`; `check_upgrades` checks that both the sum of the two ratios and the sponsored-fee ratio do not exceed 10 000; `Payload::Headers`
    gains `payee` (32 bytes, after the body list in both the Borsh and the compact encoding); shared vectors regenerated (the `Headers` vectors in `codec.json` and the "missing payee"
    invalid vector).
  - `ksana-btc`: the tracker records every landing of each block's data as `Landing { txid, height, index, sender }` (`index` is the transaction's position in the BTC block,
    `Sender::Block(containing block)` / `Sender::Payee(payee)`, `payload_sender`); `data(B)` takes the landing with the smallest (height, index); `carrier_of` decides as described above,
    `AnchoredView` (now taking the network parameters) implements `BtcView::carrier`, answering by the current chain in anchor-invalid mode; `bodies.rs` is unchanged.
  - `ksana-execution`: `BtcView::carrier` (`NoBtc` answers none), the three-way split and the capping order at unlock, the sponsored-fee record split by the rules of the unlocked block itself;
    `UnlockItem` is unchanged (the `UnlockItem.carrier` of the first draft is replaced by asking the view; the segment computation is unchanged).
  - `ksana-node` / `ksana-indexer`: the view carries the network parameters; republish payloads write `--carrier-payee` (default `--producer`, all zeros when neither is given); the rule "no funding wallet,
    no path B packing" is dropped (`producer.rs`); `locked_fees` in `ksana_getAccount` is derived by the three-way split for the carrier and the block share account, `ksana_getSettlement` gains
    `carrier_account` (answered by the current BTC best chain and the current main chain); the TS SDK's `blockCarrierAt` / `carrierFeeAt` / `blockRewardsSplitAt` (three portions) and
    `Headers.payee`. The web explorer's parameter table and settlement page are not yet updated (a follow-up for the web thread).
  - Documentation: SPEC §7.1, §7.5, §7.6, §8, §8.1, §8.5, §8.6, §13, §16, §17 have been revised per this entry; D-077 "Cost" and the payout in item 6 of D-089 are revised by this entry.
  - Local-policy fixes (the thread's option 1, implemented together with M23, not consensus rules): `ksana-mempool`'s packing candidates are ordered by fee rate (sponsored fee / cost, cost =
    max(VmOp encoded bytes, `max_gas × MAX_BLOCK_DATA_BYTES / BLOCK_GAS_LIMIT`), `op_cost` / `fee_rate`) instead of by absolute amount -- previously an op
    with `max_gas` at the block limit and a fee only 1 base unit higher could monopolize the whole block's gas; the relay floor (`--min-relay-token-fee`) and the packing floor (`--min-token-fee`,
    new `--min-fee-per-kib`) are separated (the relay floor defaults to the packing floor; when given and lower, ops that reach it enter the pool and are relayed, to be packed by other producers); `ksana_estimateFee` now returns the fee rate `path_b.per_kib`
    (the median of the per-block minimum fee rates over the last 20 blocks) and the `amount` of an ordinary transfer converted from it (`docs/RPC.md`). A single op's share of gas is not limited: once ordered by fee rate,
    large-gas ops can only pay proportionally, and a hard cap is no longer needed.
- **To be observed** (signet): the fixed part is a reward that does not vary with bytes; when KSA is valuable, producers may attach their own settlement to every block they mine to grab it rather than wait for the batch (the extra BTC fees
  are at most equal to this reward); if that appears, switch to counting by bytes (the encoded length of each block in the payload is visible on BTC). The two ratios and the 2-block wait are likewise retuned from signet observations.

## D-096 Consensus freeze: the project owner's sign-off, SPEC v2 finalized, all "pending confirmation" decisions closed out, mainnet parameters and genesis config template (signed off by the project owner 2026-10-05, mainnet default values confirmed 2026-10-06; revises the SPEC's version marker, §0, §1, §6.1, §11.3, §11.4, §12, §13, §14, §17, adds §13.1; M24)

- **Problem**: SPEC v2 (2026-10-03) merged D-001–D-086, but DECISIONS still had 29 "pending confirmation" decisions (ledger `docs/PENDING_REVIEW.md`),
  and the values of D-074, the designs of D-076 / D-077 and one default each in D-068 and D-089 were also pending confirmation; the mainnet parameters existed only as the "suggested values" in the §13 table, and the code had no mainnet network;
  §17 still listed the mainnet parameters, the issuance figures and so on as open. The project owner signed off in the project chat on 2026-10-05 17:44 UTC: "apart from the wallet that receives the [block share], I think it can be concluded".
- **Decision**:
  1. **Meaning of the sign-off**: apart from the receiving wallet of the block share, the design is final. The only open item is the receiving wallet of the block share -- the address itself is for the project owner
     to decide; the ratio and the cutoff are unchanged (D-089) [not published, D-134], not to be asked again or reopened. The settled positions of D-087 (no merged mining), D-088 (PoW fixed, no PoS), D-090
     (empty headers keep going on-chain, difficulty increasing with the streak), D-095 (carrier reward) and the like are not reopened by this entry.
  2. **All "pending confirmation" items closed out**: each item has exactly one of three conclusions -- settled (confirmed, the existing implementation stays), superseded (a later decision rewrote it), or deferred to after mainnet (with a reason and
     a trigger condition). The per-item conclusions are in `docs/PENDING_REVIEW.md` (turned into the close-out ledger), and the headings of the entries were rewritten to match; no item is left unclassified. The sign-off caused no consensus change, and
     devnet / signet do not restart. The design trade-offs 3 and 5–9 that the cleanup of implementation limitations (2026-10-04) left to the project owner are accepted as well: 3 (pruning point) is listed as a work item before mainnet,
     5–9 stay as they are; the corner case of D-056 (an anchor-invalid block whose body never lands on chain makes the indexer's finality determination pause at that height) is deferred to after mainnet.
  3. **SPEC v2 final**: the version marker becomes `v2 final · 2026-10-05`, with a version history added; the parts of the decisions after 2026-10-03, D-087 to D-095, that were not yet reflected are merged in,
     §13 now has three columns, devnet / signet / mainnet, and the new §13.1 "Parameter rationale" gives, for every consensus parameter and every per-network parameter, the mainnet value, the rationale and the status
     (settled / verified / unverified, to be measured / default value, pending the owner's confirmation); the rationale cites only existing measurements, tests and analyses, and where there are none it is honestly marked "unverified, to be measured";
     §14 adds M16–M24 and lists the non-consensus work items before mainnet; §17 is reduced to the only open item, the default values pending confirmation and the matters for after mainnet. Any consensus change after this version goes through the §6.9
     upgrade mechanism (or a new network) and is recorded as a separate decision.
  4. **Mainnet parameters**: `ksana-primitives::params` gains `Network::Mainnet` and `MAINNET` (`--chain mainnet`, `Network::from_chain_id`). The differences from
     devnet: `CHAIN_ID` 1000; `NATIVE_CONFIRMATIONS` 3 and `ANCHOR_DEPTH` 2 (the previous mainnet suggested values of §13, D-035, D-056); `POW_LIMIT_BITS`
     `0x1f00ffff` (a target of about 2^240: at the limit a block takes 65 536 Argon2id computations in expectation, at about 1 millisecond each as measured in D-094, so about 13 CPU cores suffice for one block every 5 seconds;
     the difficulty can only adjust upward from here, and the genesis config should set `bits` near the expected hashrate); KSA issuance per D-089 (`BLOCK_REWARD` 840 000 000,
     `REWARD_HALVING_INTERVAL` 12 500 000, `KSA_MAX_SUPPLY` 2.1 × 10^16, the block share [not published, D-134]; the upper bound of the total rewards is exactly the max supply,
     `max_genesis_supply` is 0). Everything else is the same as devnet (including Argon2id's 2 MiB / 1 pass / 1 lane, GHOSTDAG's 8 / 10 / 128 / 720, two-speed difficulty's
     12 / 120, the 7 200-second half-life, the carrier's 500 / 5 000 / 2, and an empty upgrade schedule). Of these, the values chosen by this entry rather than ever decided by the project owner are marked
     "default value, pending the owner's confirmation" (§13.1, §17.2): `CHAIN_ID` 1000, `NATIVE_CONFIRMATIONS` 3, `ANCHOR_DEPTH` 2, `POW_LIMIT_BITS` and the way the genesis
     `bits` is chosen, the Argon2id parameters carried over from devnet, `REORG_WINDOW` 1 000; the other values carried over from devnet rest on earlier decisions (D-013, D-063, D-084,
     D-090) and are settled. Confirming them changes no code; a change is a change to
     one field of `MAINNET`, which does not count as a consensus change before mainnet launches. **Confirmed 2026-10-06**: the project owner confirmed all six default values on the decision card
     of the consensus-freeze thread ("Confirm all"); SPEC §13.1 records them as settled and §17.2 records the confirmation; no code changed.
  5. **Mainnet genesis config template**: `mainnet/genesis.json` (described in `mainnet/README.md`): `timestamp` and `btc_anchor` are zero values, `allocations` is empty,
     `block_share_account` is the placeholder `KSANA_BLOCK_SHARE_ACCOUNT_TBD___` (32 ASCII bytes, `ksana_execution::BLOCK_SHARE_ACCOUNT_PLACEHOLDER`).
     `GenesisFile::check_network`: a genesis config of any network that carries the placeholder is rejected; mainnet (by `chain_id`) must additionally give the block share account, have no genesis allocation,
     and have a non-zero anchor and timestamp. `GenesisFile::header` runs this check first (so the node and the indexer already reject it when building the genesis block), and the node calls it once more separately before writing any data,
     so the template as it stands cannot start any network; only once the four items are filled in (share account, timestamp, anchor, genesis `bits` by the expected hashrate) can it start with `--chain mainnet`
     (and, since D-134, once mainnet's block share is published).
  6. **Work items before mainnet** (no consensus change, to be scheduled by the project owner, SPEC §14): the receiving wallet of the block share and the mainnet genesis config; a Kaspa-style pruning point (limitation 3, at the same time measuring
     the memory limit of the header DAG and extending the aarch64 determinism coverage to all consensus crates); walking path C end to end with real wallets on the public signet; a security audit; the §13.1
     measurements marked "unverified, to be measured" (the GPU advantage of Argon2id, propagation delay in a real network, calibration of the gas table); updating the web explorer's parameter table and settlement page per D-095.
- **Rationale**:
  - Only after the sign-off is the spec truly "the single source of requirements": with items hanging pending confirmation, every implementer had to go back through DECISIONS to judge which entry might still change. The way they are closed out is the most conservative --
    everything is confirmed to keep the existing behavior, and no rule is changed on the occasion.
  - The mainnet parameters go into the code rather than only into the documents so that "which values are settled and which are only defaults" has one testable source; the placeholder means that "the only open item" cannot be
    bypassed in the code either: a mainnet node whose account was not replaced does not start at all, instead of crediting the share to an account nobody has the private key for.
  - Setting `POW_LIMIT_BITS` to an easier lower bound is a choice about asymmetric risk: when the lower bound is too hard, a network with too little hashrate stalls; when it is too easy, the only problem is the ramp-up from the genesis `bits` early after launch,
    and the genesis `bits` can be configured by the expected hashrate (D-027).
  - The parameter rationale honestly marks "unverified" rather than inventing measurements: the GPU advantage of Argon2id, propagation delay at mainnet scale and the calibration of the gas table have never been measured; these are measurements to be made
    before mainnet, not something that can be settled by words.
- **Impact**:
  - Not a consensus change: the parameters, encodings and execution rules of devnet and signet are unchanged; the shared vectors and the golden roots of the determinism test are unchanged.
  - `ksana-primitives`: `Network::Mainnet`, `Network::ALL` / `name` / `from_chain_id`, `MAINNET`; tests check the mainnet values one by one, that the three chain IDs all differ,
    and that the total rewards are exactly the max supply (the exact sum falls short of the cap by exactly 1 KSA and is 0 after 30 halvings). `ksana-crypto`: `btc_network(Mainnet)` is BTC mainnet.
    `ksana-execution`: `BLOCK_SHARE_ACCOUNT_PLACEHOLDER`, `GenesisFile::check_network`, with tests that the template in the repository is rejected as it stands and can build the genesis block once filled in.
    `ksana-node`: `--chain mainnet`, the revenue address parsed for BTC mainnet, the check before building genesis; `ksana-keytool` additionally prints `mainnet_address`. `ksana-indexer`,
    `ksana-cli`: `--chain mainnet`. TS SDK: `MAINNET`, the mainnet mapping in `btcNetworkOf`. The web explorer is unchanged.
  - Documentation: SPEC v2 final, PENDING_REVIEW turned into the close-out ledger, this entry, M24 in PROGRESS, CLAUDE.md. On 2026-10-06 SPEC §13.1 / §17.2, PENDING_REVIEW, `mainnet/README.md`
    and PROGRESS were updated for the confirmed default values.

## D-097 Bounded node memory: the merge-depth root rule and pruning (decided by the project owner 2026-10-06; revises SPEC §6.3, §6.5, §6.6, §10.2, §10.3, §13, §13.1, §14, §15, §16, §17 and docs/RPC.md; consensus change, M25)

- **Problem**:
  - Node and indexer kept every block's GHOSTDAG data, the header DAG, the main chain's summaries, the BTC snapshot's landing maps, the finality decision and (indexer) every body in memory
    forever. Measured with `crates/ksana-node/examples/memory_profile.rs` (devnet parameters, release build, PROGRESS M25): 5 049 bytes per block over all of them, about 31.9 GB for a
    year of active mainnet (6 307 200 blocks at 5 seconds) and 265 MB for an idle year (52 560 empty blocks at the 10-minute idle header interval). Several operations also walked the whole
    history: main-chain selection in the header DAG scanned every selected-chain tip on every insertion, every connected block cloned the main chain's summary list, every changed BTC sync
    deep-copied the snapshot, and merging a low-work side block forked from the genesis block walked the whole chain (62 ms at 100 000 blocks, which an attacker could repeat with every
    block it sends).
  - Kaspa-style pruning drops everything below a pruning point P on the finalized chain, which is only exact when no block a chain through the finalized tip can still contain needs a
    block below P. The merge depth of D-063 only bounds a merged block's blue score (`blue_score + MERGE_DEPTH ≥ blue_score(SP)`), and blue score counts blocks, not work: a side chain
    of cheap blocks forked from an arbitrarily old block reaches a high blue score and stays mergeable, so its GHOSTDAG computation reaches arbitrarily far down. A pruned node could not
    compute such a block while a node that never pruned could: the two would judge it differently.
  - The project owner chose, on the decision card of 2026-10-06 at 10:21 UTC, "add the rule (recommended)": "a consensus change, devnet / signet restart from genesis; pruning is
    provably exact, block by block the same as a full node, and the DoS of walking a deep chain is closed at the same time".
- **Decision**:
  1. **Merge-depth root rule** (consensus, a header rule of §6.3): the merge-depth root of block B, `root(B)`, is the highest block on the selected chain of B's selected parent (the
     selected parent included) whose blue score is at most `blue_score(SP) − MERGE_DEPTH` (the genesis block when there is none). The selected chain of every block in B's merge set
     other than the selected parent must pass through `root(B)`; otherwise B is invalid (`GhostdagError::MergeDepthRoot`). The blue-score bound of D-063 stays. The header DAG
     (§6.5) applies the same rule. A walk that checks it stops where the merged block's chain meets the selected parent's chain above the root, or at the root's blue work, so the
     ancestor queries of a block's own computation stay above the root. A direct parent is checked against the root first, then against the blue-score bound (D-098) and only then
     queried as an ancestor; the root can lie up to K blue score below the bound when the selected chain's blue score jumps over it, and a parent between the two is `MergeDepth`.
     The selected parent's chain is only walked as far as the merged blocks' chains need (they coincide with it below where they meet), so a block merging recent blocks walks a
     few blocks, not `MERGE_DEPTH`; a pruned DAG reads how deep that chain is kept from a per-block record of the lowest kept block on it. A merged block's walk is bounded by
     blue work, not blue score, so a long chain of light blocks above the root is walked in full: the walks of one computation share their verdicts, each block being walked once
     whatever the merge set, and block validation (node and header DAG) checks the PoW against the header's own `bits` before GHOSTDAG, comparing `bits` with GHOSTDAG's after it
     (local policy, second audit P1-1). A block template tries the tips one by one on the main-chain tip: its computations share one merge-depth root, and a tip beyond the
     merge depth of the main-chain tip, which no block on it can merge, is not tried at all (local policy, second audit P4a-2).
  2. **Bodies carried by a settlement payload** (§6.3 body rule, D-077): besides belonging to a block in B's past, a carried body must belong to a block in the future of `root(B)` or to the
     root itself. A violation only rejects this copy (the settlement transactions are not part of the block's identity). The bound keeps the check exact on a pruned node and bounds its
     ancestor queries. It is checked once per carried block, after the past check that D-098 computes once per carrying block (a payload may carry the same body many times); D-098's
     regression test therefore carries a block 700 deep, within `MERGE_DEPTH`, since a body 2 000 deep is now rejected by the bound.
  3. **The pruning depth**: `min_pruning_depth = 3 × (MERGE_DEPTH + GHOSTDAG_K) + 1` blue score (2 209 on all three networks since D-100 raised `GHOSTDAG_K` to 16, 2 185 with D-063's K = 8; `GhostdagParams::min_pruning_depth`). A DAG pruned at a
     point P that lies this far below a block F on F's selected chain, after everything in F's past was inserted, computes every block a later chain through F needs exactly as the
     unpruned DAG: such a block's chain passes through F or through the merge-depth root of a chain block above F (at most `MERGE_DEPTH + K` below F), and its computation reaches at
     most `2 × (MERGE_DEPTH + K)` further down that chain. A computation that would need a pruned block fails with `GhostdagError::Pruned`, which is never a judgment that the block is
     invalid.
  4. **Pruning** (local policy, node and indexer alike, `ksana_btc::prune`): after a BTC sync, the pruning root R is the highest block decided BTC final at a BTC height at least
     `PRUNE_BTC_MARGIN` (144, about a day) below the BTC tip; R must be on the main chain (the node) or executed (the indexer). The pruning point P is the block on R's selected chain
     `min_pruning_depth` blue score below R; pruning happens once P is at least `PRUNE_STEP` (1 000) blue score above the current pruning point. Kept in memory: the block DAG and the
     header DAG from P on (P and its future), the finality decision with R as its root, the candidate and determined bodies of the blocks whose headers the header DAG still holds (in it or pending), the BTC snapshot from
     `anchor(P) − NATIVE_CONFIRMATIONS` on (and the landings of blocks still held), and the main chain's summaries from a window base W on, the lowest of P's height, the unlock cursor
     of R's state and one above the height the settled closure reached at that BTC cut. The main chain below W is answered from disk (the node's block store, the indexer's index).
     State versions from R's height on are kept besides the last `REORG_WINDOW`, so any main-chain switch the finality rules still allow can be executed. Each pruning writes a
     checkpoint (`pruning.ckpt` in the data directory: Borsh plus a SHA-256 checksum, written to a temporary file, synced and renamed; a BTC transaction carrying many pending payloads
     is written once, so the pending payloads share one copy of it after a restart as they do in memory, D-098), from which a restart continues instead of
     rebuilding the whole history. `--no-pruning` (node and indexer) keeps the old behavior.
  5. **Node**: the block store keeps every block; new column families `dag_seq` (every block ever inserted, also after it was pruned from memory), `summaries` (the main chain's
     summaries) and `accepted` (the main-chain height that accepted each block, with its summary) answer for pruned history, and a data directory from an older version gets them
     written back at its first start. A pruned block delivered again is a duplicate; a block whose computation needs a pruned block is not processed (not invalid). Peers syncing from
     far behind are served the main chain's acceptance order from the store, so a new node still syncs from the genesis block through a pruned peer. A restart restores the DAG, the
     header DAG, the finality decision, the bodies and the BTC snapshot from the checkpoint, checks that the store's main chain still passes through the root, and re-inserts the blocks
     stored after the checkpoint; without a usable checkpoint it rebuilds from the whole history in the store and rescans BTC from the genesis anchor. A BTC reorg deeper than what was
     kept (one that revokes R's finality or reaches below the snapshot) stops the node with an alert: it has to be resynced.
  6. **Indexer**: a new RocksDB index in the data directory (`index/`: the executed main chain's summaries and state roots by height, and the height that accepted each block) answers for
     the history below the window, for the RPC and for the carrier rule (D-095 can ask about any accepted block). An entry is written before the state of its height is committed.
     A restart continues from the checkpoint: the first round verifies the state store against the BTC data as before (D-048) and rebuilds the window from the index. An unusable
     checkpoint makes it scan BTC from the genesis anchor again; a BTC reorg deeper than what was kept, a main chain that no longer passes through the root, or a lost index makes it
     start over from the genesis anchor by itself (it needs nothing but BTC). Like the node it processes and prunes after every round of the BTC scan (at most 500 BTC blocks), so
     an indexer catching up through a long history keeps only the window as it goes; only before the verification after a restart does processing wait for the BTC tip (second
     audit P5-2).
  7. **Networks**: a consensus change before mainnet: devnet and signet restart from genesis, the D-096 route ("or starts a new network"); mainnet has not launched, so there is no
     upgrade item. The encodings, the shared vectors and the golden roots of the determinism test are unchanged.
  8. **aarch64 determinism**: the CI job that runs the determinism test on aarch64 now runs the tests of every crate that computes consensus results (`ksana-primitives`,
     `ksana-crypto`, `ksana-state`, `ksana-vm`, `ksana-execution`, `ksana-consensus`, `ksana-btc`), as the work item of SPEC §14 asked; it used to cover four of them.
- **Rationale**:
  - The rule is what makes pruning exact rather than a heuristic: with it, "pruned" and "unpruned" nodes agree block by block (property tests on random DAGs with attacker blocks,
    below), so no node can be split off by sending it a block that only a full history can judge. Honest blocks never merge a chain that misses the root (parallel blocks are merged
    within a few blocks), so the rule costs nothing in normal operation; it is the merge-depth bound of Kaspa expressed on the selected chain.
  - The same rule closes the deep-walk DoS: the walk that checks it never goes below the root's blue work, so a low-work side chain forked from an old block is rejected after about
    `MERGE_DEPTH` steps (finding the root) instead of a walk of the whole history; a side chain that reaches above the root's blue work has had that much work mined into it.
  - The pruning root comes from BTC finality because finality is what bounds which chains can still become the main chain (§6.5). The margin of 144 BTC blocks (about a day) keeps R
    final under any BTC reorg up to that depth, far deeper than any reorg in BTC's history (a few dozen blocks, 2010 and 2013); a deeper one is an emergency that needs operator
    attention anyway. The step of 1 000 (about 83 minutes of active blocks) amortizes the pruning work and the checkpoint write.
  - The node stops on a reorg deeper than the margin rather than rebuilding, because its block store does not hold the competing chain's history and its peers may not either;
    the indexer can start over because it rebuilds everything from BTC alone.
  - Syncing a new node from a state snapshot at the pruning point (Kaspa's UTXO-set sync) is not part of this decision: bounded memory does not need it, and it would need a
    commitment and proof format for the state plus a new sync protocol. New nodes keep syncing from the genesis block.
- **Impact**:
  - Consensus: `ksana-consensus::ghostdag` (the rule, `BlockDag::prune` / `checkpoint` / `restore`, `chain_floor`, `GhostdagParams::min_pruning_depth`, the selected-chain tips kept in
    an ordered index so that main-chain selection no longer scans them all); the node's carried-body check.
  - `ksana-btc`: the main chain's summaries as a window over a history on disk (`MainChain`, `MainHistory`), `BtcSnapshot::prune`, `HeaderDag::prune`, `Finality::prune` (with
    `decided_by` and `root_revoked`), `Bodies::prune`, the records the checkpoints store, `TrackerError::TooDeep`, `BtcTracker::reset`, the pruning policy and the checkpoint files
    (`prune.rs`). `ksana-node`: pruning, the checkpoint, the restart, the store's new column families, serving pruned history, `--no-pruning`; at most 16 unknown header versions are
    remembered for the once-per-version alert (that set used to grow without bound). `ksana-indexer`: pruning, the index, the checkpoint, the start-over, `--no-pruning`.
  - RPC: the merge set of a block that is not on the main chain and was pruned from memory is no longer known (`merged` is empty, docs/RPC.md); everything else answers as before.
  - Memory after the change (`memory_profile --prune`, PROGRESS M25): with the pruning root at its default distance behind the tip, memory holds a window of about 21 000 blocks of
    an active chain and about 3 000 of an idle one, whatever the chain's length; all the growing structures of the node and the indexer together hold 112 MB (active) and 16 MB
    (idle), against 31.9 GB and 265 MB per year before, and inserting a block no longer slows down as the chain grows. The window moves only while the finality decision advances:
    while it pauses (§6.5), memory grows with the blocks produced in the meantime and is released at the next pruning.
  - Tests: the rule's boundary and the low-work side chain (`ksana-consensus/tests/ghostdag.rs`); pruning equivalence on random DAGs with and without attacker blocks, the reporting of
    pruned blocks and checkpoint round trips (`tests/pruning.rs`); the BTC side's pruned structures against unpruned ones and their records; the node (`tests/m25_pruning.rs`: prunes
    as finality advances, the DAG holds only the pruning point and its future, restart from the checkpoint, a node that never pruned syncs everything from a pruned one and agrees, a
    pruned block delivered again is a duplicate, a body below the merge-depth root is rejected by a pruned and an unpruned node alike); the indexer (`tests/pruning.rs`: matches the
    node at every height while pruning, restarts from the checkpoint, a corrupt checkpoint and a lost index both converge).

## D-098 Security audit of 2026-10-06: adversarial simulations, exchange partial claims and local hardening; consensus fixes proposed to the project owner (2026-10-06; revises SPEC §5.3, §6.8, §7.3, §7.7, §10.3, §10.4, §14, §15, §16, adds §17.4; the example exchange contract and local behavior only, no consensus change, M26; the proposed consensus fixes are adopted by D-100)

- **Problem**: the work items before mainnet include a security audit (SPEC §14), and M19 left two items of the §15 verification gap: an empty-block strategy / selfish-mining analysis, and the cost of lock-without-pay in the
  exchange. The project owner asked on 2026-10-06 "what is left 🤔 an audit? a test run?" and then "do 1 and 3 first", which started the pruning point (1) and this item (3): the two
  adversarial items first, then an internal audit of `main`. An external audit (firm and budget) is the project owner's call.
- **Method**: six static review passes over `main` ef07411 (encoding and signatures; consensus, GHOSTDAG, header DAG and finality; the BTC layer; execution, state and VM; node, P2P, RPC and entry pool; indexer, CLI, SDKs, contracts and the
  web wallet's key handling). Every finding was traced through its callers before it was graded, and every local fix starts from a test that fails, or measures the bad behavior, on the old code. The two simulations run on the real fork
  choice and GHOSTDAG of `ksana-consensus`. Result: 42 findings (F-01 to F-42: 3 critical, 6 high, 8 medium, the rest low or informational) and three simulation results (S-01 to S-03). The report (Chinese, plain-language version
  first), the findings list and the six pass reports were kept outside the repository.
- **Decision**:
  a. **Payment reversal against heavy empty blocks (S-01, `ksana-consensus/tests/reorg_race.rs`)**. The attacker mines a private branch from the payment block's parent, its first block carrying a conflicting op, and wins when the fork
     choice prefers its branch at some moment between the merchant's acceptance and BTC finality (one hour). Four attacker strategies (weight-1 blocks, empty blocks by its own streak, empty blocks only while behind plus one weight-1 block
     to make its heavy tip count) against a busy chain (honest blocks weight 1), an idle chain (12, 24, 48, 96, 120, ...) and a local-policy variant (honest producers keep weight 1 for 2 or 5 minutes after a payment); 20 000 races per
     cell; sample races replayed through the real `BlockDag` pick the same tips. Results (best strategy per cell, α = 10%): busy chain 4.0% after 30 seconds, 3.5% after 1 minute, 0.61% after 5, 0.24% after 10, 0.045% after 30; idle chain
     76%, 73%, 36%, 23% and 8.2%. With every block of weight 1 a 10% attacker gets 0.77% after 30 seconds, 0.07% after 1 minute and nothing measurable after 5. The idle chain is weak because honest work arrives in lumps that take minutes and
     a block's own work does not count in the fork choice until a block is built on it; the busy chain is weaker than the weight-1 reference because an attacker's lucky empty block is worth up to 12 honest blocks. The local policy of
     staying at weight 1 after a payment helps only against weight-1 attackers (0.045% after 5 minutes) and not against empty-block ones (5.8%). No rule change: SPEC §10.4 gains the table and the guidance (small amounts only on a
     fast-lane confirmation; about 5 minutes on a busy chain for moderate amounts; BTC finality on an idle chain or for large amounts).
  b. **Withholding against heavy empty blocks (S-02, `selfish_mining.rs`)**. A miner hides the weight-1 blocks it finds (with ops of its own, which D-087 allows) while the honest miners work on a heavy empty block, and publishes them
     when that turns the honest blocks found meanwhile red: more than `GHOSTDAG_K` hidden blocks in the honest block's anticone, and a hidden chain the fork choice prefers because the heavy block's own work does not count yet. Every block goes
     through the real `BlockDag`, rewards are tallied from the final selected chain and its coloring (red blocks get nothing, §8.6), 2 400 simulated hours per cell. On an idle chain a 5% / 10% / 20% / 30% miner turns 30% / 45% / 56% /
     64% of the honest reward slots red; its own blocks stay blue and its absolute income is unchanged (the DAA score counts red blocks, so the difficulty does not fall), its share of the slots paid rises to 7% / 17% / 36% / 54%, and
     every success restarts the honest streak, so more headers go to BTC. On a busy chain it does not pay (nothing at 10%, 0.16% of the honest slots at 20%, 2.4% at 30%, with losses of its own). Publishing weight-1 blocks at once is not
     an attack (each side is paid its hashrate share) but raises the headers an idle chain lands from 6 an hour to 71 (5% miner) to 255 (30%). The options, each a consensus change, go to the project owner (SPEC §17.4): count a tip's own
     work in the fork choice (closes the window, and lowers idle-chain reversal to 13% after 5 minutes and 7.6% after 10 for α = 10%, but raises busy-chain reversal after 1 minute from 3.5% to 6.5%); pay merged empty blocks even when
     red (the simulated honest loss drops to 0, but a failed reversal attempt with heavy blocks then costs nothing); or document only. Not recommended: lowering `MAX_EMPTY_BLOCK_WEIGHT` (changes D-090's numbers), or honest producers
     capping their own empty weight with ops of their own (with a cap of 24 a 10% miner still takes 4.8%, and an idle chain lands 58 headers an hour instead of 6). Neither simulation merges anything deeper than
     `MERGE_DEPTH`: the withheld branches of S-02 are a few dozen blocks, and S-01 is decided by the fork choice, which the bounded merge depth of M25 (D-097) leaves unchanged; both are re-run on top of D-097 once it is on `main`.
  c. **The exchange: partial claims keep the lock open (F-06)**. Anyone can claim a lock with a path C op of their own in a transaction paying the lock's script, and a claim used to close the lock and return the unsold rest to the seller.
     So the seller could confirm a few sats to its own payment script before the buyer's payment was force-included (the claim of a path C payment executes only `NATIVE_CONFIRMATIONS` BTC blocks after it confirms, and until then
     `get_lock` says `Open`): the buyer got 3.3% of the KSA for the full price, the seller the BTC and the rest (reproduced in `contracts/examples/ksa-exchange/tests/adversarial.rs`). Now a claim buys `min(paid_sats × price,
     KSA still locked)` for the beneficiary and leaves the rest locked; the lock is `Claimed` (with the totals) once all of it is bought, and released after expiry otherwise; a BTC transaction can claim a lock once (state prefix `t`, key
     lock and transaction ID), and the KSA bought so far is kept under prefix `f`. Nothing returns to the seller at claim time, so its dust only buys KSA for the buyer. The contract's code hash changes: the example exchanges deployed on
     devnet / signet have to be redeployed. The CLI, the TS SDK (`topUp.ts` no longer describes the pre-D-086 risk), the web simulator and the UI copy follow; SPEC §5.3.
  d. **Lock without pay and script squatting (S-03, F-21)**. A buyer who asks for a lock and never pays spends nothing on chain; the seller pays the sponsored fees of `lock` and `release` and cannot use the KSA until the anchor reaches
     `expiry + RELEASE_DELAY`, so even the shortest lock keeps it for `RELEASE_DELAY + 1` BTC blocks (about 70 minutes on mainnet). The contract cannot make the buyer pay for that; short expiries, an off-chain deposit or reputation are the
     seller's tools. Someone who sees a seller's `lock` op in the entry pool can lock 1 base unit with the same payment script first, so the seller's lock fails ("script already used by a lock"): it costs the squatter its fee and gains
     it nothing, and the seller retries with a fresh address; clients resolve locks by id and check their terms, never by script. Both are documented (§16), not changed.
  e. **Local fixes** (no consensus change: which blocks are valid, what they execute to, encodings, state roots and the data sent to BTC are unchanged):
     1. **BTC tracker memory (F-03, critical)**: for every body a payload carries of a block not known at scan time the tracker kept a deep copy of the carrying transaction, so one ~80 KiB republish payload with thousands of junk
        headers and bodies cost hundreds of MB to GB on every node and indexer, kept until the blocks became known and rebuilt at every start; `resolve` also copied the whole snapshot before any lookup. The pending payloads of one
        transaction now share one copy, the snapshot is copied only once a pending block turns out to be known, and a run of pending payloads sharing a transaction is decoded once (1 000 carried bodies: 1 copy instead of 1 000 × 37 807
        bytes; 1 500 repeated bodies resolve in 79 ms instead of 59.8 s in a debug build).
     2. **GHOSTDAG check order (F-04)**: `BlockDag::compute` runs before a header's PoW is checked, and the ancestor query of each non-selected parent walked the selected chain down to that parent before the merge depth was checked, so a
        header naming the tip and an old block cost a walk of the whole chain, for free, under the node's processing lock. A direct parent beyond the merge depth is invalid either way (`ParentInPast` or `MergeDepth`), so it is now rejected
        as `MergeDepth` before the query (on a 50 000-block chain 0 steps instead of 49 999; a randomized test shows identical verdicts and GHOSTDAG data in both orders and against a set-based implementation of §6.3).
        With D-097 the merge-depth root is checked before both, so a parent below the root is `MergeDepthRoot` without a walk, and the depth check decides the parents between the root and
        the merge depth.
     3. **Orphan pool and carried bodies (F-05, F-10)**: an orphan was kept without any check (no PoW, any number of parents) and the pool was capped only by count, while a decoded entry takes 304 bytes against 13 on the wire, so a
        few 16 MiB messages exhausted memory. Every block's parent list is now checked first (at most `MAX_BLOCK_PARENTS`, distinct, GHOSTDAG's errors, the hash marked invalid as before); an orphan is kept only if its later parents
        ascend, its PoW meets its own `bits` (a valid target within the PoW limit) at the lowest weight it can have and its entries match `entries_root` (a failure rejects the copy without marking the hash); the pool is also capped at
        `MAX_ORPHAN_BYTES` (32 MiB, by encoded length plus the in-memory size), dropping the oldest first. The bodies a settlement payload carries are checked against the carrying block's past computed once instead of one walk per body
        and parent (499 bodies of a block 2 000 deep: 446 ms before, 20 ms now; the same verdicts).
     4. **Queued gossip of banned peers (part of F-28)**: messages a peer had queued before its ban were still decoded and validated; the validation tasks now drop them unvalidated (10 of 10 validated before, 4 of 10 now).
     5. **Entry pool (F-12)**: a full pool (10 000 ops) rejected every newcomer and never removed ops that can never be packed, so about 160 dust accounts queuing gapped 64 KiB ops could fill every pool for good and lock out all new
        path B ops. Now `max_bytes` (32 MiB) caps the encoded bytes; a full pool evicts an account's last op, from the account whose last op stands lowest (behind a nonce gap, then unfunded, then below the packing floor, then packing
        candidates; by fee rate and fee within each), and rejects a newcomer only when it would go first; `max_gapped_per_account` (8) limits the ops behind a nonce gap; after chain changes the pool drops the ops the balance no
        longer covers (with the account's later ops) and the ops the current rules no longer accept. (D-104 item h: the nonces authenticated path C ops waiting for forced inclusion take in turn from the account's nonce on, in BTC order,
        are no gap.)
     6. **Indexer (F-08, F-23)**: the indexer halted on a main-chain block whose body, or a merged block's body, broke the state-independent entry rules or the forced-inclusion check. Since it picks its main chain from the headers on BTC
        and executes a block after `SETTLEMENT_DEPTH` confirmations, a republish payload with a few valid-PoW headers and one broken signature halted every indexer until restart. The nodes reject such a block and every block after it,
        so the indexer now disqualifies it like a block that cannot land and re-selects the main chain, halting only when the block is BTC-final (D-082; SPEC §10.3 item 5). After a restart with bitcoind behind the kept state versions,
        the verification found no version to compare and rebuilt from genesis; it now waits until the processable prefix reaches the oldest kept version (§10.3 item 8).
     7. **Block production and settlement liveness (F-01 and F-02 local parts, F-11, F-14, F-38)**:
        - Merging (F-02): a block accepted while its anchor was off the BTC best chain (anchor-invalid, D-056) has unchecked forced-inclusion entries, and one naming an unknown transaction made every producer's lenient
          execution fail with a local fault on every attempt. A template now leaves out a tip that would bring into the merge set a block anchored on the current chain without its window's forced-inclusion prefix, one anchored
          above the local BTC tip, or an anchor-invalid one whose body the indexer could not determine (`ksana_btc::bodies`: without a body, entries other than those the current chain derives for its window; with one, a forced
          entry that is not a native transaction on the current chain carrying the same op). When building still fails while executing the merged blocks, the producer finds the failing block by bisection and leaves it out until
          the BTC tip changes; a failure that cannot be pinned on one block stays a local fault.
        - Body size (F-01): forced-inclusion entries are mandatory and unbounded (§7.4), and the producer still added the due unlock entry and the waiting headers and bodies, so production failed on every attempt once the
          settlement payload exceeded `MAX_BLOCK_DATA_BYTES`. Unlock and path B entries are now added only while this block's body fits in a republish payload carrying just its header and body (`ksana_btc::payload::body_room`);
          a settlement payload that would be too large carries only this block's data, or nothing for a block with only forced-inclusion entries (it needs no body).
        - Republish size (F-11): republish payloads were sized without their 32-byte payee (D-095), so a long run of empty-block headers came out a few bytes over the limit and was never sent ("insufficient funds" in the log). They
          are now sized with `headers_payload_fixed_len` and trimmed by their exact encoding.
        - `ksana_call` (F-14): the caller's `max_gas` was passed through (a looping contract ran three times `BLOCK_GAS_LIMIT` in the test) and calls ran without limit. `max_gas` above the next block's `BLOCK_GAS_LIMIT` is now
          refused (`-32602`), and a node runs at most 4 calls at once (`-32009`, `docs/RPC.md`).
        - Rescue claims (F-38): any mempool transaction whose payload ended with the stuck block's header held every node's rescue back for as long as it stayed in the mempool. A claim now counts only when it is a commit / reveal
          pair carrying exactly the block's payload, its commit spends confirmed outputs only and it pays at least this node's fee rate, and only for `CLAIM_PATIENCE` (`RESCUE_ORDER_WINDOW + RESCUE_DELAY`) BTC blocks after this
          node first saw it (§7.3).
        - Tests: `ksana-node/tests/adversarial_liveness.rs` (11, each failing or measuring the bad behavior on the old code) and two unit tests in `ksana_btc::payload`.
     8. **TS SDK (F-18)**: the compact entry decoder dropped a leading U+FEFF from method names (`TextDecoder` default), so the SDK threw on settlement payloads Rust accepts; it now keeps it.
  f. **Consensus fixes proposed** (SPEC §17.4; the project owner adopted all of them with the first option of S-02 on 2026-10-06, implemented in M27 by D-100; before the mainnet launch a change means a devnet / signet restart
     from genesis and its own decision entry):
     1. **F-01**: a block that needs a body is valid only if its body fits in a republish payload carrying just its own header and body (`body_room`, payee included). Today any miner can produce, at the cost of one
        block, a block holding two 64 KiB sponsored ops of its own (fee 0, `max_gas` 0) whose body no payload can carry: the settled closure, every unlock, BTC finality and the indexer stop at it for good, and path B pauses after
        `SETTLEMENT_TIMEOUT`; without hashrate, about 2 450 path C transactions in one BTC block plus an honest unlock entry do the same unless producers drop what does not fit (item e.7).
     2. **F-02**: a new block may name an old parent whose anchor lags more than `MAX_ANCHOR_LAG + ANCHOR_DEPTH` and a made-up anchor hash at that height; delivered by sync it is accepted as an anchor-invalid historical block (§6.8) and
        its forced-inclusion entries go unchecked. A forced entry naming a made-up transaction made every producer's merge of it fail with a local fault, forever (mitigated locally by item e.7); one naming a confirmed non-native
        transaction is merged, but the indexer can never determine that block's body and stops there. Proposed: in lenient mode a forced entry that does not resolve to a native transaction carrying the same op on the merging block's
        anchor chain is skipped; an anchor-invalid block is accepted only if its forced entries resolve by transaction ID on the current chain to native transactions carrying the same ops (the indexer's conversion rule); §6.8's
        rationale corrected. The local mitigation only keeps honest templates clear of such a block: a block another miner builds on it can still make a node's switch to that chain fail with a local fault, and only the rule
        closes that. The bounded merge depth of M25 (D-097) narrows the old parents such a block can name but does not close it.
     3. **F-09**: the header DAG rejects a header whose anchor is at or above the BTC height of the payload that carries it (today the timestamp bound is skipped for such a header, and the finality determination can pause on it).
     4. **F-16**: payload decoding checks `MAX_SETTLEMENT_HEADERS`, as §7.1 already says.
     5. **F-40**: settlement transactions must use the producer's `nSequence` on every input (a BIP68-locked settlement delivered by sync forces a rescue today).
     6. **S-02**: one of the three options in item b.
     Not in this list: repricing code compilation (F-07) goes with the gas table calibration of §13.1.
  g. **Documented, not changed**: rescue cost (F-13: a rescue copies the payload, so the carrier reward stays with the original producer while the rescuer pays; §7.3); the one-way protection of the BIP322 message (F-19: a site asking
     for a BIP322 signature of a "random" 64-hex string can present a VmOp sighash; wallets must show the decoded op; §16); the header DAG's timestamp-bound verdicts are not re-checked after a BTC reorg deeper than payload height minus
     anchor height (F-26, the rarity class of D-056); the rescue change text of §7.7 (F-31, the code was right); a settlement payload's carrier is credited by its last header, which BTC data cannot tie to the block that really carries it,
     so a miner holding any accepted block can race a producer's own settlement for the carrier share without the republish wait (F-41, D-095).
  h. **Open local hardening** (not done in M26, no consensus change): F-07 compile amplification (benchmark cold compiles of adversarial 64 KiB modules, then keep compiled modules for all deployed code or at least use LRU); F-15 top-up
     trusts the exchange contract and node it is given (pin the reviewed code hash per network, confirm URL-supplied endpoints, cap the fee rate); F-17 a copied path C op hides the real op's status; F-20 the contract SDK's Merkle proof
     accepts the last transaction of an odd level at two positions (CVE-2012-2459; contracts must key replay protection on the transaction ID, now stated on `verify_tx_inclusion`); F-22 CLI top-up fee rate, deadline and expiry check; F-24 the web mock wallet's key in the
     query string; F-25 / F-39 unbounded sets and chain-length work per block (with the pruning point); F-27 `GhostdagError::Corrupt` mapped to invalid in the header DAG; the rest of F-28 (connection limits, sync server concurrency,
     op messages decoded with the 16 MiB limit, per-peer orphan budget; all but the orphan budget closed by D-132); F-29 RPC batch and body limits (closed by D-132); F-30 secrets on the command line and key files; F-32 VM event pricing, `limit_hit`, bulk-memory and `memory.grow` cost
     tests; F-33 super-linear compact header decoding; F-34 the TS `decodePayload` default network; F-35 indexer candidate bodies; F-36 duplicate block messages; F-37 the stale-anchor retry loop; F-42 log flooding.
- **Rationale**:
  - Local fixes where the consensus is untouched, proposals where it is not: the consensus is frozen (D-096), and a consensus change before mainnet still restarts devnet / signet and is the project owner's call. Each proposal in item f
    only invalidates blocks an honest producer never makes, except S-02, which is an economic trade-off and therefore offered with its costs.
  - The indexer change keeps its role as the source of final truth: it still never accepts an invalid block, it only stops treating "an attacker landed one" as "the fast lane is broken"; a BTC-final invalid block still halts it.
  - The exchange fix keeps the lock flow of D-085 (lock, pay and claim, release after expiry) and its single-beneficiary guarantee, and leaves nobody who profits from closing a lock early. A claimer check (only an account the buyer
    names in the lock terms may claim) would stop the front-run too, but the buyer would have to fix its claiming account before the lock, which changes the order flow of D-078 / D-085 in the CLI, the SDK and the web wallet;
    partial claims fix it inside the contract.
- **Impact**:
  - No consensus change; devnet and signet do not restart; the shared vectors and the golden roots are unchanged. The example exchange contract's code hash changes.
  - `ksana-consensus` (merge depth before the ancestor query; two simulation tests), `ksana-btc` (tracker), `ksana-mempool`, `ksana-p2p`, `ksana-node`, `ksana-indexer`, `contracts/examples/ksa-exchange`, `ksana-cli`, TS SDK, web app.
  - Documentation: SPEC (version history, §5.3, §6.8, §7.3, §7.7, §10.3, §10.4, §14, §15, §16, §17.4), `docs/RPC.md`, PROGRESS M26, CLAUDE.md, this entry.

## D-099 No Chinese name: the brand is "Ksana" in every language, and the only Chinese left in the repository is the web page's Chinese UI (decided by the project owner 2026-10-06; documentation, web page and test data; rewords SPEC's version history, §1 and §17 without changing a rule; no consensus change)

- **Problem**: the English conversion of 2026-10-06 kept some Chinese on purpose: the Chinese brand name (in SPEC, WEB_DESIGN, PROGRESS, CLAUDE.md, the web page's title, description,
  wordmark and Chinese, Japanese and Korean copy, and a test vector), the owner's words quoted verbatim in the documents next to their English translation, the Chinese glossary columns of WEB_DESIGN §8.3, Chinese test inputs
  (the shared vectors, one case of the official BIP322 vectors, unit tests) and a few Chinese strings in the web code (among them the BTC RPC client's errors, the language menu and the end-to-end script). The web page also drew the Han
  character for "eternity" on the vermilion seal of "BTC final", and in Chinese, Japanese and Korean set small marks for "the instant" and "eternity" beside the stages. The project owner on 2026-10-06: "Delete every
  occurrence of the Chinese name, and let no Chinese appear. Let them choose a Chinese name themselves." Asked how far this goes, the owner chose to keep the web page's Chinese interface.
- **Decision**:
  1. **The brand is "Ksana" in every language.** It has no official Chinese name and is not translated or transliterated in any UI language; the name's origin is given as the Sanskrit kṣaṇa (SPEC §1, WEB_DESIGN §1.2).
     The Chinese, Japanese and Korean copy that used the name or its pun on "the instant" and "eternity" is rewritten in plain words (the overview title, the title of the operation's progress, the tagline).
  2. **Where Chinese may appear**: only in the web page's Chinese UI language packs (`web/src/i18n/zh-CN/`, `zh-TW/`); the Japanese and Korean packs stay. Nowhere else in the repository (CLAUDE.md, "Language"):
     - Documents: the owner's words quoted in DECISIONS, PROGRESS, PENDING_REVIEW and SPEC keep only their English translation, worded as before. WEB_DESIGN §8.3 keeps the English and Spanish terms; the Chinese,
       Japanese and Korean terms live only in their message files, and a new message reuses the term its file already uses.
     - Web page: the wordmark is "Ksana" in the bundled serif in every language; the page's `<title>` and description are English, and `<html lang>` is `en` until the app sets the language. "BTC final" is the
       meander-pattern seal in every language (English and Spanish already used it, D-071), and the small marks beside the stages are removed (the messages `status.markFast` / `markFinal` / `sealGlyph` go).
       The BTC RPC client's three Chinese errors (`web/src/wallet/btc.ts`) become messages (`rpc.btcRpcNotJson` / `btcRpcFailed` / `btcRpcHttp`), and the full-width colon its UTXO parser accepts is written
       as `\uff1a`; each language's name in the language menu moves into its own `name.ts`; the top-up errors become per-language functions (the full-width colon hard-coded in every language goes).
       The check for a wallet's refusal (`isWalletRejection`, `web/src/wallet/connect.ts`) still recognizes the Chinese words for "reject" and "cancel" in a wallet's error, written as code points.
       The heading-font subset is regenerated from the current copy, and its character list (`ksana-serif.chars.txt`, which only repeated the Simplified Chinese copy) is no longer kept in the repository.
       The end-to-end script `web/scripts/e2e-devnet.mjs` drives the page in English.
     - Test data: the Chinese strings in test inputs are replaced with non-Chinese multi-byte text: the method names `überweisen` (its near miss in the signing vectors `überwiesen`), `mint_ñ_kṣaṇa` and `échanger`,
       and `non-ASCII — é ₿ 😀` in the bitcoind error parser's test. `tests/vectors/codec.json`, `signing.json` and `merkle.json` are regenerated with `gen_vectors`, and only the values that depend on these strings
       change (the method name, the Borsh bytes, `op_id`, `sighash`, the signatures, `entries_root`). The third `tx_hashes` case of the official BIP322 vectors (`crates/ksana-crypto/tests/data/bip322/` and
       `sdk/ts/test/data/bip322/`, the two copies identical) had Chinese words in its message; the message becomes `UTF-8 support: öäüéàè ₿ € 😄` and its three hashes were recomputed with an independent script that
       reproduces all three upstream cases; both test files note the local change.
  3. **Not changed**: the git history and the titles and bodies of earlier pull requests.
- **Rationale**: the owner leaves a Chinese name to the people who use it rather than fixing one; one name everywhere also keeps the brand the same on every page and in every language. A test that needs non-ASCII text
  only needs multi-byte UTF-8 (two-, three- and four-byte sequences), which the new inputs still cover.
- **Impact**: no consensus change: the encodings, rules, parameters, genesis configs and the golden roots of the determinism test are unchanged; the regenerated vectors differ only where a test input changed, and Rust
  and the TS SDK check against the same files. The web page shows "Ksana" in every language, and in Chinese, Japanese and Korean no longer shows the Han seal or the stage marks.

## D-100 Consensus fixes of the security audit: the fork choice counts a tip's own work, a body fits in a payload, forced-inclusion entries of anchor-invalid and merged blocks, merged unlock entries, payloads with headers anchored at or above their own BTC height, `MAX_SETTLEMENT_HEADERS` when decoding, the `nSequence` of settlement transactions, `GHOSTDAG_K` 16 (decided by the project owner 2026-10-06; revises D-056, D-063, D-091, D-097, D-098 and SPEC §6.2–§6.6, §6.8, §7.1–§7.6, §8.5, §9.6, §10.3, §10.4, §11.1, §13, §14–§17; consensus change, M27)

- **Problem**: the security audit of 2026-10-06 (D-098 item f, SPEC §17.4) found five holes that only a consensus rule closes, each letting one block or one BTC payload stop block production, the settled closure, BTC finality or the
  indexer (F-01, F-02, F-09, F-16, F-40), and an economic weakness of the fork choice: on an idle chain a miner withholding weight-1 blocks turned honest heavy empty blocks red (S-02), and an idle-chain payment was weakly protected (S-01).
  On the decision card in the audit thread the project owner chose on 2026-10-06 to fix them all and to change the fork choice: the five rules and option a of S-02 (count a tip's own work in the fork choice), in a milestone of
  their own after M26 and on top of the bounded merge depth of M25 (D-097), so that devnet and signet restart from genesis once. On a second card the project owner chose on 2026-10-06 to raise `GHOSTDAG_K` to 16 against
  what option a leaves, withholding a heavy empty block (item g). A second internal audit the same day, of M25 and the first M27 commits, found that items b, c and d as first written could still stop the settled closure
  for good with one block, and that whether a node accepted an anchor-invalid block could depend on how much it had pruned (A2-01, P4b-1, P4b-2, P6-2); the items below include its fixes, which stay within what the cards chose
  (rationale), and item h lists the local policy that goes with them. On a third card the project owner chose on 2026-10-07 to always skip unlock entries of merged blocks (item c (4)) rather than
  keep the rule as it was.
- **Decision**:
  a. **The fork choice counts a tip's own work** (S-02). `chain_work(B) = blue_work(B) + work(B)`. The selected parent, the main chain (§6.4), the order in which a template takes tips (§6.2), the finality candidate `best_H`
     (§6.5), the competing-chain trigger of D-091 (§7.6) and sync's `Status` (`tip_work` and the order of the tips, §11.1) compare `(chain_work, hash)` (`BlockDag::chain_work` / `chain_key` in `ksana-consensus`). `blue_work` stays as
     GHOSTDAG defines it, and the sort key `(blue_work, hash)` stays where an order of blocks rather than a choice between tips is wanted: the order of a merge set, the order sync hands blocks out in, where an ancestor query stops, and the
     producer's choice of headers to carry (§7.5). Both keys strictly increase along every parent-child edge (`blue_work(B) ≥ chain_work(SP) ≥ chain_work(P) > blue_work(P)` for every parent P), so both are topological orders.
     With the bounded merge depth (D-097): the index of selected-chain tips from which the main chain is searched is ordered by `(chain_work, hash)`, and the search still stops at the first tip
     not above the best candidate found, since chain work strictly increases along a selected chain.
     Simulated on the real fork choice and GHOSTDAG with D-063's K = 8 (`reorg_race.rs`, `selfish_mining.rs`, which keep the old rule for comparison; item g gives the K = 16 numbers):
     - Withholding weight-1 blocks on an idle chain no longer pays: the honest loss falls from 30% / 45% / 56% / 64% of the reward slots (α = 5% / 10% / 20% / 30%) to 0 / 0 / 0.03% / 1.2%, and the attacker loses 31% / 55% / 63% / 50% of its
       own income (its blocks merged too late are red) instead of nothing.
     - Payment reversal (best of three strategies, 50 000 races per cell, α = 10%; after 30 seconds, 1, 5, 10 and 30 minutes): idle chain 43%, 33%, 13%, 7.5%, 2.5% (before: 76%, 73%, 36%, 23%, 8.2%); busy chain 7.4%, 6.4%, 0.83%,
       0.32%, 0.03% (before: 4.0%, 3.5%, 0.61%, 0.24%, 0.045%): on a busy chain an attacker's empty block is worth 12 honest blocks and now counts as soon as it is found. SPEC §10.4 has the table (with K = 16); the guidance (small amounts
       only on a fast-lane confirmation, about 5 minutes on a busy chain for moderate amounts, BTC finality on an idle chain or for large amounts) stays.
     - What remains is **withholding a heavy block**. The attacker keeps an empty block (weight 12 on a busy chain) private, extends it with weight-1 blocks and publishes just before the honest blocks found meanwhile overtake it; those
       beyond the first `GHOSTDAG_K` turn red. With D-063's K = 8, on a busy chain a 5% / 10% / 20% / 30% attacker took 1.5% / 3.6% / 11% / 23% of the honest slots without losing any of its own blocks (under the old rule the same
       strategy took 0.47% / 1.8% / 7.3% / 17% and cost the attacker 27% / 14% / 2.4% / 0% of its income, since its heavy block needed one more block on top before it counted); on an idle chain 1.6% / 5.4% / 13% / 20% (before:
       2.9% / 7.8% / 17% / 25%). It works because an empty block outweighs more than `GHOSTDAG_K + 1` honest blocks (12 against 9); item g narrows it.
  b. **A body fits in a payload** (F-01). A block that needs a body (§6.4) is invalid unless a republish payload carrying only its header and its entries, with a 32-byte payee, is no longer than `MAX_BLOCK_DATA_BYTES`
     with every key of the entries counted as written in full: `headers_payload_fixed_len([header]) + max_body_len(entries) ≤ MAX_BLOCK_DATA_BYTES` (`ksana_btc::payload::check_body_fits`). That upper bound is how every
     node measures a body it lands for someone else (carrying and republishing, §7.5, §7.6); measured by the exact encoding, where a key already in the payload's dictionary costs one byte, the rule let through bodies no node
     would ever land (second audit P4b-2). The node checks it right after the entries root (the block is marked invalid) and before it keeps an orphan, the indexer when it checks a body. Honest producers already packed to
     this bound (`body_room`, D-098 item e.7). A block with only forced-inclusion entries needs no body and is not bound by it (how many native transactions it must include is up to BTC, §7.4).
  c. **Forced-inclusion entries of merged and anchor-invalid blocks** (F-02). (1) `BtcView::native_input(txid, op)` (formerly `native_first_input(txid)`) answers `None` when the anchor chain has no native transaction with that
     ID up to the anchor height, or one carrying another op, instead of failing; `forced_prepare` turns that into an invalid entry, which makes the block invalid in strict mode and, merged, makes the merging block invalid too
     (§7.4, §9.6): the merging block then finds the same transactions when it is first executed and when it is executed again in anchor-invalid mode. The first version skipped such a merged entry, and a re-execution could
     find a transaction the first execution did not (second audit A2-01). Honest producers do not meet it: they anchor at `max(tip − ANCHOR_DEPTH, parent anchor)` and leave a block whose merge would fail out of their templates
     (D-098 item e.7). In anchor-invalid mode the answer comes from the current best chain, where a transaction carrying another op is likewise `None` and a missing one stays a local fault (below). (2) A block
     accepted in anchor-invalid mode (D-056) must have entries the indexer can determine from the current best chain (`ksana_btc::bodies::check_stale_entries`): in a block that needs a body every forced-inclusion entry names a native
     transaction of the current best chain carrying the same op, confirmed above the block's window start `h(parent anchor)` and at or below `h(tip − ANCHOR_DEPTH)` (`ksana_btc::bodies::stale_range`, the window end of a block a
     producer would anchor now; another op: block invalid; no such native transaction in that range: this copy is rejected, since the transaction may gain the confirmations or be confirmed again); a block with only
     forced-inclusion entries has exactly the entries the current best chain gives over its window (block invalid otherwise). The range starts at the window start, which every node and indexer still processing the block
     keeps (its parents anchor at or above the pruning point's anchor, D-097), so pruning does not change the verdict; the first version looked at every height the local snapshot kept (second audit A2-01). The node checks it
     when it accepts such a block, the template's merge-hazard check (D-098 item e.7) uses the same function, and the indexer converts a candidate body's forced-inclusion entries over the same range (§10.3 item 3). §6.8 is
     corrected: the anchor-invalid threshold alone does not stop a forged anchor; these rules keep such a block from stopping block production or the indexer. (3) Executed again in anchor-invalid mode, a block's BTC view
     answers from the current best chain, which may not reproduce the first execution: a verdict that the block is invalid is then a local fault with an alert, like a `state_root` that does not match (§6.8; second audit P6-2).
     (4) Unlock entries of merged blocks are always skipped: only a main-chain block's own unlock entry advances the cursor, so a block's state never depends on how its BTC view judges a merged block's unlock entry, an answer
     that can differ when the block is executed again in anchor-invalid mode (§9.6; second audit P6-2). Producers take their unlock target from the parent state's cursor, so honest blocks do not change. What remains is a
     BTC reorg of `ANCHOR_DEPTH + 1` blocks or more (3 on mainnet) after a block was accepted that drops or moves what its execution relied on (a native transaction, a data landing): a node or indexer executing the block
     again (one syncing from genesis, a deep rewind) stops on a local fault with an alert until the BTC chain holds it again, instead of computing another state; nodes already past the block are not affected (§17.3).
  d. **A payload counts only when its headers anchor below its own height** (F-09). The audit proposed rejecting such a header in the header DAG. Done that way, whether a header entered the DAG would depend on which payload a node or
     indexer saw it in first and on whether it already knew the header from the fast lane, so two honest observers could disagree. The rule is on the payload instead (`ksana_btc::tracker::payload_counts`): a settlement or republish
     payload confirmed at BTC height h counts only if every header it carries anchors below h, and one that does not count is ignored as a whole while scanning (its headers are not seen, its bodies do not land, its sender carries
     nothing, `observe_payload` is not called). An honest payload always counts: its blocks anchor to BTC blocks that existed when they were produced, and it confirms later. Every header in the header DAG then anchors to a BTC block
     already scanned, so its timestamp bound (D-083) is checked when it appears. The settlement payload a block carries (§7.2) may carry only headers anchored at or below the block's own anchor height
     (`settlement_belongs_to`): its reveal transaction commits to the block's anchor and so confirms above it, and the payload then always counts on a chain that holds the anchor. Without this, an attached payload could
     carry a header that keeps it from ever counting, and the block's data would never land while every node took that payload for its settlement (second audit P4b-1). The headers of an honest payload are in the block's
     past, and anchors never decrease along parent edges.
  e. **`MAX_SETTLEMENT_HEADERS` when decoding** (F-16). `Payload::decode` reads the header count and refuses more than `MAX_SETTLEMENT_HEADERS` before reading any header (`compact::read_headers(reader, max_count)`, likewise
     `decodePayload` in the TS SDK), as §7.1 already said; republish payloads are bound too (§7.1 and the type docs said "arbitrary headers"), and the node's republish stops at the limit. Producers carried at most
     `MAX_SETTLEMENT_HEADERS − 1` other headers anyway.
  f. **The `nSequence` of settlement transactions** (F-40). Every input of the commit and the reveal transaction has `nSequence = 0xFFFFFFFD` (`PRODUCER_SEQUENCE`, what producers always used; `validate_attached`): with another value
     BIP68 could put a relative time lock on an input, and a block delivered by sync could carry a settlement that cannot land until the lock expires, forcing a rescue.
  g. **`GHOSTDAG_K` 16** (what item a leaves). With K = 16 the honest weight-1 blocks that a withheld empty block of weight 12 can overtake fit in a blue anticone. Simulated (`selfish_mining.rs`, 2 400 hours per cell):
     a 5% / 10% / 20% / 30% attacker withholding a heavy block takes 0 / 0.02% / 1.1% / 10% of the honest slots on a busy chain and 0.50% / 2.9% / 9.6% / 16% on an idle chain, where the honest blocks are empty blocks found
     minutes apart and the private chain can still put more than K blocks in the anticone of one of them; it loses none of its own blocks. Withholding weight-1 blocks costs the attacker a tenth to almost half of its own
     income (K = 8: a third to two thirds), and on a busy chain the honest loss at α = 30% falls from 2.4% to 0.61%. Payment reversal moves by at most half a percentage point (busy chain, α = 10%, after 5 minutes:
     0.83% → 0.91%; SPEC §10.4 has the K = 16 table). At δ = 10⁻³ (P[Poisson(2Dλ) > K] < δ, λ = 0.2 blocks / second) 16 tolerates a propagation delay bound D of about 17.6 seconds, against 6.1 for 8. The other
     levers simulated, at α = 10% (busy / idle): `GHOSTDAG_K` 12, 0.58% / 4.0%; `EMPTY_BLOCK_DIFFICULTY` 9 or 6 with K = 8, 1.1% / 5.4% and 0.08% / 5.4% (a lower N does not help an idle chain and puts more empty-block
     headers on BTC). The pruning depth of D-097 becomes 3 × (720 + 16) + 1 = 2 209 blue score (2 185 with K = 8). Mainnet has not launched, so this is one field of `MAINNET` (§17.2); devnet and signet take it at the same restart.
  h. **Local policy that goes with the second audit's fixes** (no consensus change). (1) A settlement that confirmed on BTC without counting (a BTC reorg mined it again at or below its block's anchor height, §7.1) has not
     landed the block's data: rescue treats it as stuck, and redoes its own rescue that confirmed that way (§7.3; second audit P4b-1). (2) A block delivered by sync and refused for a reason that is not objective is answered from
     memory while the BTC tip stays the same, for at most 30 seconds: for the block when the reason holds for every copy (its anchor, an anchor-invalid block's forced-inclusion entries), for that copy only when the reason is
     the copy's settlement transactions; and the body checks against the BTC view and the settlement transactions come before the signature checks. One sync response could deliver such a block hundreds of times, each
     costing the proof of work, GHOSTDAG, BTC lookups and signatures again (§11.1; second audit P4a-1). (D-104 item j: a block missing a parent refused for itself is refused with it
     while that refusal is remembered, and remembering such a refusal drops the orphans waiting for the block.)
- **Rationale**:
  - Items b to f only invalidate, or stop counting, what an honest producer never makes; item a changes which tip wins, and the project owner chose it with its cost in view (the decision card named the higher busy-chain reversal at short
    waits). Against the other options of D-098 item b: paying red empty blocks would have left the reversal numbers as they were and made a failed reversal attempt with heavy blocks free; documenting only would have left honest
    miners of an idle chain losing up to half their reward slots to a 10% miner.
  - Item d is on the payload rather than the header so that node and indexer decide the same way whatever order they learn blocks in (determinism, §2).
  - The second audit's fixes keep to what the cards chose: each only invalidates or stops counting what an honest producer never makes (b: producers already packed to the upper bound; c: producers anchor so that their
    merges find the transactions, and take their unlock target from the parent state; d: an honest attached payload carries headers from the block's past), or changes local policy (h).
  - Item c (4) was the recommended option of the third card: executing a block again no longer depends on how its BTC view judges a merged block's unlock entry, and the rewards and fees are unlocked in the same
    amounts by the main-chain blocks' own entries; keeping the rule as it was would have left one more case in which a node or indexer executing a block again after a BTC reorg stops with an alert (§17.3).
  - Item g was the recommended option of the second card: of the levers simulated it is the only one that also helps an idle chain, and its costs are small (coloring checks up to K + 1 blue blocks per merge set, about twice
    the worst-case work of 8; the pruning depth of D-097 grows by 24 blocks). It does not close the idle-chain case, which stays recorded in SPEC §16.
- **Impact**:
  - Consensus change: blocks the old rules accepted can be invalid now (a body too large, forced-inclusion entries of an anchor-invalid block not on the current chain, a settlement with another `nSequence`, an attached
    payload with a header anchored above the block, a merged forced-inclusion entry naming no native transaction), payloads that counted may not, merged unlock entries no longer change the state, the fork choice can pick
    another tip, and with K = 16 a block with 9 to 16 blue blocks in its anticone can now be blue. devnet and signet restart from genesis, together with D-097. The encodings, the shared
    vectors (`tests/vectors/`) and the golden state roots of the determinism test are unchanged.
  - Code: `ksana-consensus` (`chain_work`, `chain_key`, the selected parent, the index of selected-chain tips), `ksana-primitives` (the header count when decoding, `GHOSTDAG_K`), `ksana-btc` (`check_body_fits`, `check_stale_entries` and
    `stale_range`, `payload_counts`, the header anchors of an attached payload, `AnchoredView::native_input`, `check_sequences`, the header DAG's and the finality determination's keys), `ksana-execution` (`BtcView::native_input`,
    `forced_prepare`, merged forced-inclusion and unlock entries), `ksana-node` (fork choice, templates, `check_body` / `check_orphan` and the order of the body checks, the refusals remembered while syncing, re-execution in
    anchor-invalid mode, competing chains, `Status`, republishing, rescuing a settlement that did not count), `ksana-indexer` (body fit, the stale range, re-execution in anchor-invalid mode, main chain), `ksana-p2p` (`Status` docs),
    TS SDK (the header count when decoding), the web simulator (a parallel block is never heavier than the chain block at its height; merged unlock entries are skipped) and parameter table.
  - Tests: unit tests for each rule (`ksana-btc`: `check_body_fits` by the upper bound, the header anchors of an attached payload, `check_stale_entries` and a lookup that does not depend on pruning, the scan of a payload over
    the header limit and of one with a header anchored at its own height, settlement `nSequence`; `ksana-primitives` and the TS SDK: decoding over the header limit; `ksana-consensus`: the selected parent by chain work, the
    index of selected-chain tips by `(chain_work, hash)`; `ksana-execution`: a merged forced entry naming no native transaction makes the merging block invalid, merged unlock entries are skipped), node tests (a body too
    large for a payload is invalid, also as an orphan; a heavy tip outweighs a branch with more blue work; an anchor-invalid block whose forced entries do not resolve is rejected or invalid, and accepted once its transaction
    has the confirmations; a settlement confirmed without counting is rescued; refusals remembered while syncing and the order of the body checks) and an indexer test (a forced-inclusion transaction confirmed again above a
    reorged-out anchor is still found); the two simulations on the new rule, whose quick withholding test checks the K = 16 numbers.
  - Documentation: SPEC (version history, §6.2–§6.6, §6.8, §7.1–§7.6, §8.5, §9.6, §10.3, §10.4, §11.1, §13, §14, §15, §16, §17), `docs/WEB_DESIGN.md`, PROGRESS M27, CLAUDE.md, this entry; D-098 item f is settled by this entry.

## D-101 Only a block judged valid becomes BTC-final: the finality determination asks for the verdict of executing the next candidate and excludes an invalid one (requested by the project owner 2026-10-08; item c corrected and item i added after the two reviews of the change the same day; revises D-056, D-082, D-093, D-097, D-098, D-100 and SPEC §6.2–§6.5, §6.8, §10.2, §10.3, §15, §16, §17.3; consensus change, M28)

- **Problem**: the path assessment of 2026-10-07 (finding 1) found that BTC-anchored finality (D-082) never asked whether a block is
  valid. The determination took the heaviest candidate among the blocks whose headers had appeared on BTC and made it final once the data of its closure had `BTC_FINALITY_CONFS` confirmations; an invalid block that got
  there first became final like any other. Nothing could heal that: the main chain must pass through a finalized block, so the indexer halted with an alert, the node's determination waited forever on a block it had
  refused (SPEC §6.5 item 6), nothing could become `btc_final` again, and only a new release could get the network going. Together with the honest side's fixed settlement fee rate (A2-09, D-102), the assessment put
  the cost at one ordinary fast-lane block plus one commit / reveal pair (about 40 000 sats at 100 sat/vB), plus about 0.13 BTC of filler transactions when the attacker has to create the congestion that keeps honest
  data off BTC for `BTC_FINALITY_CONFS` blocks; neither internal audit had put the two together. On 2026-10-08 the project owner asked to think through how finality should be handled, then make the changes and go
  through the usual process again. The assessment's order puts this consensus rule first (item 1.1) and the fee policy of D-102 second (item 1.2).
- **Decision**:
  a. **The rule** (§6.5 item 2). At BTC height H, when block C on the walk along `best_H`'s selected chain meets the landing condition (and item c has not excluded it), the verdict of executing C as a main-chain
     block on top of its selected parent, the finalized tip F, is asked (item b). Valid: C is final and F advances to it. Invalid: C is **excluded** at H together with every block whose selected chain passes through it, `best_H` is chosen again among
     the other candidates and the walk goes on at the same height; a block that merges C without passing through it on its selected chain stays a candidate and gets its own verdict. No verdict yet: the
     determination pauses at H, as for a landing status it cannot determine (§6.5 item 3). An exclusion is recorded with the BTC block of its height and revoked with the decisions on a BTC reorg (§6.5 item 4).
  b. **The verdict** (`ksana_btc::verdict`, shared by node and indexer; §6.5 item 6): the body rules of each block C merges, in consensus order, and of C (the state-independent entry rules under the rules at the
     block's own DAA score, D-076; the body fitting in a payload, D-100; forced inclusion when the block's anchor is on the BTC best chain, §7.4); the settlement backlog limit on C (§6.4); then the merged blocks
     executed leniently and C's own entries strictly, and C's `state_root` (§9.6). This is how the indexer executes a main-chain block, which it now does through the same function. The verdict is asked only once
     C's closure has landed with `BTC_FINALITY_CONFS` confirmations and C's selected parent is the finalized tip, never earlier: it then depends only on the BTC chain up to C's anchor height (C and every block it
     merges anchor at or below it), so every node and indexer on the same BTC chain decides the same, however early it learned that a block is invalid.
  c. **Blocks anchored off the BTC chain** (§6.8, D-056, D-100 item c (3); corrected after the first review of the change, see the rationale). When the walk at H reaches C, C is excluded at H, before its landing status
     or its verdict is asked, if the anchor of C or of a block C merges is not on the BTC best chain: the BTC block at that anchor height is another one (an anchor hash no BTC block has, or a block that a BTC
     reorg orphaned). A payload counts only when every header it carries anchors below its own height (D-100), so these anchors lie below the height at which C's header first appeared, hence below H: the
     exclusion depends only on the BTC chain up to H, and every node and indexer on that chain excludes the same blocks (`Exclusion::Unanchored`). The verdict is then asked only about blocks whose own anchors and
     whose merged blocks' anchors are on the BTC chain, so it never executes in anchor-invalid mode.
  d. **Recorded verdicts.** A verdict is recorded with its basis, the hash of the best chain's BTC block at C's anchor height, and used again only while the basis is unchanged; after a BTC reorg reaching below
     the anchor the block is judged again. The header DAG's marks on blocks that cannot land (D-063) carry the same basis, and the marks on a newly finalized chain whose basis changed are dropped before the floor
     moves (`HeaderDag::requalify`). The node and the indexer also keep the verdicts on disk (a column family `verdicts`: the version of the verdict rules, `verdict::VERDICT_VERSION`, then Borsh
     `(valid, basis)`), so that a restart need not judge every block again; it is a cache, and a lost verdict, or one kept under another version of the rules, is judged again (the version was added after
     the first review of the change, so that a release changing what the verdict checks does not reuse the old answers).
  e. **Node.** A main-chain block it connected while its anchor was on the BTC best chain, with every block it merges accepted the same way by this process, has its verdict from that connection, which checked
     everything the verdict checks. Otherwise the node judges C on top of the finalized tip's state without committing anything (`StateDb::preview_root_at`), from its own copies and, for a block it lacks or
     refused, from the body determined from the data on BTC (§10.3 item 3). An invalid C is excluded even when this node never had it; a valid C becomes final once the node has it (rebuilt from BTC or supplied by
     a peer, D-093), so that its main chain can then pass through it. The node keeps every state version from the finalized tip's height on (§10.2). Without pruning, a BTC reorg deeper than
     `BTC_FINALITY_CONFS` can move the finalized tip below the states kept: the node then retreats its main chain to the finalized tip, rebuilding the state from the genesis block (D-093), and judges on top of it.
     With pruning the states from the pruning root on are kept, which covers every finalized tip such a reorg can leave; a deeper one stops the node for a resync (D-097).
  f. **Indexer.** It executes every main-chain block through the shared verdict: a valid one is committed, an invalid one disqualified (D-098). When the determination waits on a block whose selected parent it
     has executed but which is not on its processable prefix yet, it judges the block on top of that state without committing. It also keeps every state version from the finalized tip's height on.
  g. **Checkpoints.** The exclusions are part of the finality determination saved in `pruning.ckpt`; its local format version becomes 2 for the node and the indexer, and a version 1 checkpoint is not used (BTC is
     rescanned from the genesis anchor).
  h. **A finalized block that is invalid** (§6.5 item 6) can now only come from a non-determinism bug: the indexer still halts and the node still keeps its main chain, each with an alert.
  i. **Keeping blocks anchored off the BTC chain off the main chain** (node, §6.2, §6.3, §6.4, §6.8; added after the second review of the change, see the rationale). A node takes such a block from a peer (D-056):
     the sync branch of `Chain::check_anchor`, which handles every sync response, every orphan taken after its parent and every block rebuilt from BTC, accepts an anchor that lags the tip by more than
     `MAX_ANCHOR_LAG + ANCHOR_DEPTH` without looking at its hash, and any stale block bitcoind knows. Three rules keep such a block from stopping the determination: (1) templates never merge a block anchored off
     the BTC best chain, nor a tip whose past holds one (`Chain::merge_hazard`, local policy, M26; the `check_stale_entries` fallback remains only for an anchor below the BTC snapshot's base); (2) the main-chain
     selection never takes onto the main chain a block that item c would exclude under the current BTC chain, for its own anchor or for that of a block it merges (`Chain::best_candidate`, the same test as the
     determination): such a block stays a side block and nothing is built on it, while acceptance is unchanged; (3) a block taken from a peer may not be anchored above `max(parents' anchor height,
     tip − ANCHOR_DEPTH)` either, the same bound as for a new block (retry later): without it a block anchored at the BTC tip could join the main chain through sync, and a 1-block BTC reorg would then exclude it.
     An orphan refused for now is logged and skipped, so that the block it waited on keeps its report of main-chain changes, and a block rebuilt from BTC that is refused for now is
     tried again at the next BTC sync without failing this one (with rule 3 and `ANCHOR_DEPTH` 2, a block whose header landed in the BTC tip block anchored one below it, sent with an orphan
     child, otherwise made every BTC sync fail until BTC advanced, skipping pruning; with `ANCHOR_DEPTH` 0 or 1 it cannot happen, as a payload counts only when its headers anchor below it).
- **Rationale**:
  - Execution depends only on its inputs, which come from the BTC chain (§2), so validity is as much a function of BTC data as landing is: node and indexer still compute the same finalized chain.
  - Asking only at the landing point and only on top of the finalized tip makes the question the same for everyone: a node that refused the block when it arrived and an indexer that never saw it judge the same
    execution.
  - Excluding the selected-chain future of C but not the blocks merging C: a block whose selected chain passes through C is executed on top of C's state and can never be on a main chain; a block merging C executes
    C leniently and is valid or not on its own (D-063).
  - Pausing while no verdict can be given is the conservative choice, as for an undetermined landing status: no block is decided final, or excluded, on a guess.
  - Item c as first written paused instead: a failed execution in anchor-invalid mode was no verdict, and the determination waited at that height until the BTC chain changed, on the reasoning that only a BTC
    reorg below C's anchor after its data landed could lead there. The first review of the change showed that no reorg is needed: nothing checks an anchor's hash before a
    header counts (`payload_counts` checks only the height; the header DAG checks the anchor's height against the parents' and, when it can be obtained, the anchor block's time), so anyone can land a block whose
    `btc_anchor` names no BTC block. Its re-execution fails the same way at every BTC tip (a wrong `state_root`, or a forced-inclusion entry naming a transaction that never confirms, which is a local fault in that
    mode), so the determination of every node and indexer waited at that height for good: the headers landing above it never connected, the honest chain could not take over, pruning stopped with it (its root
    comes from `decided_by`) and the states kept from the finalized tip on grew without bound; only a new release could recover. Without any execution the same happened when such a block's body cannot be
    determined (its forced-inclusion entries cannot be derived on its anchor chain, D-093), at C itself or at a block C merges: its landing status stayed undetermined (§6.5 item 3). The cost was that of the
    assessment's attack: one fast-lane block and one landed transaction pair while the honest data stays off BTC for `BTC_FINALITY_CONFS` blocks, which with D-102 takes fees above the honest side's cap for that
    long.
  - Excluding by the anchor alone depends on nothing above H. Counting a failed anchor-invalid execution as invalid would not: that execution reads the current chain at any height, so a forced-inclusion entry
    naming a transaction that confirms after H would make an observer judging at H exclude C and a later one finalize it.
  - Item c alone moved the stall instead of removing it. The second review of the change showed that no BTC transaction is needed to put a block
    item c excludes under the honest chain: a node takes such a block from any peer (item i), the template merged it whenever its body could be determined, which an empty block or one with only path B
    entries always allows, and the main-chain selection knew nothing of the exclusion, so a block on the tip became the main-chain tip and the producers built on it. Once an honest block merged such a block or
    was built on it, no honest block could become final again, on every node and indexer, with pruning stopped and the states kept growing; only a new release could recover. The review confirmed four ways,
    each for one or two fast-lane blocks and a P2P connection: an empty block on an old main-chain block with a made-up anchor hash, merged by the next honest block; the same plus a block on the tip merging it;
    a block on an idle tip whose own anchor lags, with a made-up anchor hash (routine on an idle chain); and a block on the tip anchored on a BTC block a 1-block reorg just orphaned. D-101's own deep-reorg node
    test stalled the same way once extended by a few landed rounds, with no attacker. Item i closes all four; the node and indexer suites pass with it.
  - **Alternative not taken** for item i: refusing at acceptance a block that merges a block anchored off the chain (the review's first proposal). It leaves out a block on the tip whose own anchor is off the
    chain, which a node takes as a historical block (the idle-tip and stale cases); keeping acceptance as it is (D-056) and such blocks off templates and off the main chain covers all four.
  - **Known limitation**: no BTC reorg is needed to get a block anchored off the BTC chain into a node's DAG (the sync branch of `Chain::check_anchor` and the stale blocks bitcoind knows, item i), and
    the three rules of item i keep such a block out of templates and off the main chain. What remains is a block that is already on the main chain when a later BTC reorg of at least `ANCHOR_DEPTH` + 1 blocks
    (3 on mainnet, 2 on signet, 1 on devnet) orphans its anchor, or the anchor of a block it merges. Such a block can then never be final, and neither can any block whose selected chain passes through it.
    The main chain is not re-selected away from it, so the determination stops advancing, and pruning stops with it. Meanwhile every node and indexer keeps every state version from the finalized tip on (§10.2)
    and prunes nothing, so its state store and memory grow with every block until a branch without the block becomes final. A node that syncs that history afterwards keeps the block off its main
    chain, while the older nodes stay on it, until a branch without the block becomes final and forces them over (D-082). The producers that had the block keep building on it, so such a branch
    appears only once a producer's main chain leaves the block out (a
    producer synced after the reorg, say); meanwhile a node started after the reorg does not follow the producers, and the indexers, whose main-chain selection (the header DAG's) does not apply item i's test,
    follow the older nodes. `m6_1_regtest` and the BTC-reorg tests of `indexer.rs` show it with a 1-block BTC reorg on devnet. **Follow-up**: re-select the main chain when a BTC reorg excludes a block on it
    above the finalized tip, in the node and in the header DAG alike; the fast lane then forks below that block. Done by D-104 (M29).
  - **Alternative not taken**: judging a block anchored off the chain on a BTC view frozen at H (only the chain up to H, no headers of a stale branch from the local bitcoind, a transaction not confirmed by H
    counted as absent, the verdict's basis the block at H) would make its failure a deterministic invalid verdict and let an honest block re-anchored by a shallow reorg still become final. It needs a second
    mode of `AnchoredView`, H in the verdict callback, another basis for the verdict cache, and the same freezing for the body determination of anchor-invalid blocks, whose undetermined landing status stalls the
    determination before any verdict is asked: much more code on a consensus path, for a case that needs a reorg deeper than `ANCHOR_DEPTH`.
  - The cost: the determination now waits for execution, and a non-determinism bug that used to halt every indexer would now split the finalized chain between observers; the two-process determinism test and the
    aarch64 determinism job (D-097) remain the guard.
  - Considered and not done: raising `BTC_FINALITY_CONFS` (assessment item 1.3) would give the honest side a longer window but make every finality slower, while D-102 addresses the window directly; a simulation of
    the BTC fee market and the landing race (assessment item 1.4) is left for the measurements before mainnet (§14).
- **Impact**:
  - Consensus change: where the old rule finalized an invalid block, another chain becomes final. devnet and signet restart from genesis. The encodings, the shared vectors and the golden state roots of the
    determinism test are unchanged.
  - Code: `ksana-btc` (`finality.rs`: the verdict asked, exclusions with their reason (`Exclusion`), the anchor rule of item c, `Outcome::validating` / `excluded`, `Finality::decided_by` ignoring decisions above the heights evaluated; `verdict.rs`; `headers.rs`: marks with
    their basis, `requalify`), `ksana-state` (`StateDb::preview_root_at`), `ksana-node` (the verdict asked of the chain, `judge_for_finality`, verdicts from connection, the `verdicts` column family, the states kept from
    the finalized tip, `retreat_to_finalized`, the landing status of blocks it lacks by the indexer's rules; the rules of item i in `merge_hazard`, `best_candidate` and the sync branch of `check_anchor`, the
    orphan loop of `accept_block` and the rebuild loop of `rebuild_missing_from_btc`), `ksana-indexer` (execution through the shared verdict, judging a candidate off the main chain, the `verdicts` column family, the states kept from the finalized tip),
    checkpoint format 2.
  - Tests: unit tests of the determination (an invalid candidate is excluded and the next best becomes final, the verdict is asked only once the landing condition holds, a missing verdict pauses, a BTC reorg
    revokes the exclusions, a block merging an excluded block can become final, a decision at a paused height counts only once the height is evaluated, a block anchored off the BTC chain or merging one is
    excluded before its landing status or verdict is asked, and a BTC reorg onto its anchor revokes that exclusion), of the verdicts on disk (read only under their version) and of `preview_root_at`; indexer tests (an invalid block landed on BTC is never final, for a running indexer, a fresh one and the node; a block missing
    a forced entry is disqualified and never final; a restart with bitcoind behind takes the verdicts from the index; from the first review of the change, a block anchored on no BTC block is excluded and finality goes on,
    whether its body landed with a wrong `state_root` or never landed, and so is a block merging one, each run against the same run with the real anchor hash; from the second review, `off_chain_anchors.rs`:
    a block anchored off the BTC chain handed over through sync is never merged or built on and finality goes on, for the four ways of the rationale, each against the same run with the real anchor);
    a node test (a BTC reorg undoing finality below the states kept makes the node retreat, rebuild the state and follow the new finalized chain; since item i extended until finality passes the next block, which
    no longer merges the blocks the reorg orphaned the anchors of). Changed by item i: a fresh node syncing history that holds blocks anchored on an orphaned BTC block keeps them, and the blocks on top of them,
    off its main chain (`m6_1_anchor`, three tests of the indexer); an anchor-invalid side block is not merged and the producer force-includes its transaction itself (`adversarial_liveness`); a historical block
    anchored above the tip − `ANCHOR_DEPTH` waits like a new block (`m6_1_anchor`), and so does a block rebuilt from BTC, without failing the BTC sync (`m18_btc_rebuild`).
  - Documentation: SPEC (version history, §6.2–§6.5, §6.8, §10.2, §10.3, §15, §16, §17.3), PROGRESS M28, CLAUDE.md, this entry.

## D-102 The node's BTC fees follow the market: an estimated rate within a floor and a cap, CPFP bumps through the P2A output, a block's own waiting settlement treated like a claim, the next-block rate while a competing chain is on BTC (2026-10-08, with D-101; revises D-045, D-057, D-091, D-098 and SPEC §7.2, §7.3, §7.5, §7.6, §7.7, §11.2, §15, §16, `docs/RPC.md`; local policy, no consensus change, M28)

- **Problem**: the second internal audit's A2-09 (medium, to fix before mainnet; `audits/2026-10-06-second-internal-audit.md`): rescue skipped a block whose own attached settlement was in the BTC mempool, with no
  fee-rate or patience check, so a producer that attached its settlement at the minimum relay fee during a fee spike left the block's data unlanded, and the backlog limit stopped path B on every node after
  `SETTLEMENT_TIMEOUT`. More generally nothing ever raised a fee: the rate was the fixed `--settlement-feerate` (default 2 sat/vB), `keep_alive` only rebroadcast, and CPFP through the reveal's P2A output was
  possible only by hand. The path assessment's finding 1 (D-101) showed the same gap as a security problem: while honest data cannot land, a lighter chain an attacker lands at a higher fee becomes final, and
  D-091's republish paid the same fixed rate.
- **Decision**:
  a. **Fee policy** (`FeePolicy` in `ksana-node`): every BTC transaction the node sends (its settlements, rescues, republishes, funding splits and CPFP children) pays bitcoind's `estimatesmartfee` for the
     confirmation target (`--settlement-conf-target`, default 3 BTC blocks, conservative mode), kept within the floor `--settlement-feerate` (default 2 sat/vB, never below `MIN_SETTLEMENT_FEERATE`) and the cap
     `--max-settlement-feerate` (default 100 sat/vB, never below the floor); the floor when bitcoind has no estimate (a fresh regtest) or cannot be asked. The estimate, in BTC/kvB, is converted to sat/vB
     rounding up, without floating point.
  b. **Racing a competing chain** (assessment item 1.2): while a lighter competing chain is on BTC (D-091, within `BTC_FINALITY_CONFS` BTC blocks), the target is the next block: settlements, republishes and bumps
     then pay to land before the other side's data reaches `BTC_FINALITY_CONFS` confirmations, and so do rescues; a funding split keeps the policy's own target (it does not race; changed after the review of
     M28). The cap still applies: past it, the operator has chosen not to pay more.
  c. **CPFP bumps**: a commit / reveal pair this node sent (the settlement of a block it produced, a rescue, a republish) that has waited in the BTC mempool for `BUMP_AFTER` (2) BTC blocks (at once while a
     competing chain is on BTC) and pays less than the current rate gets a child spending the reveal's P2A output (240 sats, empty witness) and one funding UTXO of this node (the smallest that pays), with the change
     back to the funding address, lifting the package of pair and child to the rate: the child's fee is `(pair vsize + child vsize) × rate − pair fee`, at least the child's own minimum (`settlement::cpfp_child`).
     When the rate rises again the child is replaced (full RBF, the default since Bitcoin Core 28) by one spending the same funding UTXO and paying at least the old fee plus its vsize at 1 sat/vB (BIP125 rules 3
     and 4). The child's funding input stays reserved until the child confirms or is gone. A child that left the mempool is sent again only while its pair still waits there; once the pair no longer waits
     (it confirmed without the child, or is gone) the child is given up, since it would only pay the package's deficit to sweep the P2A output (the review of M28).
  d. **The block's own settlement waiting in the mempool** holds the rescue back on the same terms as another party's claim (D-098): only while the pair pays at least this node's rate, and for `CLAIM_PATIENCE`
     BTC blocks after this node first saw it waiting; then the block counts as stuck and the rescue order of D-057 starts. The producer never rescues its own waiting settlement; it bumps it (item c). The block's
     own settlement is no longer also counted as another party's claim when the mempool is scanned, which would have restarted its patience.
  e. **`ksana_estimateFee`**: `feerate_sat_vb` is the policy's current rate (outside a race), no longer the fixed setting.
  f. **Mock BTC** (tests): `MockBtc::set_fee_estimate` and `MockBtc::set_min_feerate`; `mine` takes only the ancestor packages that pay the minimum, like Bitcoin Core's block assembly, so a child can pay for its
     parents.
- **Rationale**:
  - The audit's suggestion and the assessment's item 1.2 as written: an estimated instead of a fixed rate, bumping this node's own pairs, the block's own pair treated like a claim, bidding within the finality window when
    racing.
  - CPFP rather than replacing the pair: the block carries both transactions, and the settlement records and mempool checks go by the reveal's ID (§7.2); the P2A output exists for this (D-081).
  - Only this node's own pairs are bumped: bumping another producer's settlement would pay its fee for it; rescue lands that data at this node's rate once it waits too long.
  - Defaults: the cap of 100 sat/vB bounds what a fee spike, or an attacker filling BTC blocks, can make an operator pay per virtual byte; the target of 3 keeps ordinary landing within about half an hour;
    `BUMP_AFTER` of 2 matches the in-flight wait of settlement batching (D-077).
- **Limitations**:
  - Another node judges a waiting pair by the pair's own fee rate and does not count a CPFP child, so it may still rescue a bumped settlement after its delay: an extra rescue costs fees, not liveness.
  - **P2A pinning** (the review of M28): the reveal is a version 2 transaction and its P2A output can be spent by anyone, so anyone can attach a large child at a low rate first (up to the mempool's
    descendant limits, about 100 kvB). This node's child must then pay more than that child's whole fee (BIP125 rule 3), so `testmempoolaccept` refuses it every round and the pair stays at the pinned
    package's rate. The data then lands only through a rescue by another node, which builds a new pair that can be pinned the same way, and a lone producer never rescues its own pair (item d). Closed by
    D-131 (2026-10-28): the payload of a pinned pair goes out again in a new pair of this node's, which nothing of the old pair's holds back.
  - A pair that never entered the mempool (below bitcoind's dynamic minimum) is not bumped: rescue rebuilds it at the current rate after its delay. Package relay (`submitpackage`) is not used.
  - Above the cap nothing more is paid: an attacker who keeps BTC blocks full above every operator's cap for `BTC_FINALITY_CONFS` blocks still keeps honest data off BTC; that is the case SPEC §6.5 accepts (a lighter
    chain landed in time wins), now with a price the operators set.
  - The fork block of A2-21 (open) now costs each funded node a republish at the next-block rate, up to its cap, instead of at its floor.
  - The mock has no RBF: the replacement of a child is covered by the unit test of `cpfp_child`'s minimum fee, not by a node test.
- **Impact**: local policy only; new flags `--max-settlement-feerate` and `--settlement-conf-target`; `BtcClient::estimate_feerate`; `settlement::cpfp_child`; `Rescuer::bump`. Tests: `ksana-btc` (the CPFP child, the
  fee-rate conversion, the mock's package mining and estimates); node (the fee policy; a settlement left behind by a fee spike is bumped and mined with its child; a waiting settlement paying less than this node's
  rate is rescued in its turn; one that waits too long is rescued after `CLAIM_PATIENCE` and its turn; a child left behind by its confirmed pair is not sent again). Documentation: SPEC §7.2, §7.3, §7.5, §7.6, §7.7, §11.2, §15, §16, `docs/RPC.md`, PROGRESS M28, CLAUDE.md,
  the status of A2-09 in the second audit's report.

## D-103 Bounded node memory: capped RocksDB write buffers, one block cache per process, jemalloc on Linux (2026-10-08, jemalloc 2026-10-11; revises SPEC §10.2, §10.3, §16; local settings, no consensus change, M25)

- **Problem**:
  - The 10-hour devnet soak of `main` 1ea0150 on the project owner's arm64 Mac (2026-10-08) pruned four times, and yet the node's resident memory kept rising, from about 150 MB at the first pruning to 166–192 MB at the
    end; the indexer's stayed at 80–97 MB. Pruning bounds the node's own structures (D-097), so the growth was elsewhere.
  - Measured in one process (a scratch test, not kept: one node on the mock BTC chain at devnet parameters with pruning and settlement batching on, one transfer per block and a BTC block every two fast-lane blocks, sampling
    every 500 blocks the node's Rust heap apart from the mock chain's, glibc's arenas and RocksDB's own counters): from 5 500 to 12 000 blocks the resident memory rose from 94.8 to 141.1 MiB while the node's Rust heap stayed
    at 12.8–14.7 MiB and glibc's free memory at about 13 MiB. The growth was RocksDB's write buffers (memtables), 79 MiB at 12 000 blocks and growing by about 6 MiB per 1 000 blocks.
  - Cause: the databases were opened with RocksDB's defaults (RocksDB 11.8). A column family's write buffer is written to a table file only once it holds 64 MiB, and nothing bounds the write buffers of a database in all;
    each column family also creates a block cache of its own (32 MiB), and the index and filter blocks of the table files stay in memory outside any cache. The node writes 17 column families (10 of the block store, 7 of
    the state store), a few KiB per block over all of them, so no write buffer filled for days of blocks: they kept every write since the process started, bounded only by 64 MiB for each of the 17, about 1.1 GB
    in all. The indexer writes 9 (7 of its state store, 2 of its index), with fewer bytes per block. (D-101 has since added a column family of verdicts to the block store and to the index: 18 and 10.)
  - With the bounds, three node binaries connected over libp2p still grew after pruning (a scratch test, 2026-10-09: one fake bitcoind, a BTC block every two fast-lane blocks, transfers through one node's RPC,
    20 000 blocks): 95–98 MiB per node at 4 000 blocks, 122–126 MiB at 20 000. RocksDB stayed within the bounds below (write buffers 16–25.5 MiB, block cache under 2 MiB at every pruning) and glibc's memory in
    use stayed at 39–48 MiB; what grew was the free memory glibc kept in its arenas, from 40 to 61 MiB. glibc gives threads that allocate at the same time arenas of their own (about 22 per process here) and
    returns little of what they free to the system; a node on its own, with fewer threads, did not show it.
- **Decision** (`ksana_state::rocks`; every database of the node and the indexer is opened with `DbMemory::open`):
  1. A column family's write buffer is flushed at 4 MiB (`CF_WRITE_BUFFER_BYTES`), and once the write buffers of a database hold 16 MiB in all (`DB_WRITE_BUFFER_BYTES`), RocksDB flushes the column family with the
     oldest one.
  2. The databases of one process share one LRU block cache of 32 MiB (`BLOCK_CACHE_BYTES`): the node's block store and state store, the indexer's index and state store (also when it rebuilds the state store from the
     genesis block). The index and filter blocks of the table files are kept in it, those of level 0 pinned, as RocksDB recommends with index blocks in the cache.
  3. Compression stays off, now set for every column family (the RocksDB build has no compression library, so its default was already none). The format on disk is unchanged and existing data directories open as before.
  4. The node and the indexer log what the write buffers and the block cache hold at every pruning (`RocksDB memory: write buffers … KiB, block cache … KiB`).
  5. On Linux the node and the indexer allocate with jemalloc: `tikv-jemallocator` is the global allocator of both binaries, built with `unprefixed_malloc_on_supported_platforms` so that jemalloc also
     replaces `malloc` and serves RocksDB. Other platforms, the CLI, the keytool and the test binaries keep the system allocator.
- **Rationale**:
  - These are local settings: they change when RocksDB writes to disk and what it keeps in memory, never what is stored or read, so no result and no consensus rule depends on them.
  - At a few KiB per block the bounds cost little: the busiest column family, the state store's tree nodes, fills 4 MiB about every 3 000 blocks of one transfer each, and the state store fills 16 MiB about every 3 500.
    A write buffer being flushed still counts until its table file is written, so the write buffers of a database can briefly exceed the bound (the unit test allows twice it): those of a process stay within about
    64 MiB, and held 18–27 MiB in the measurement below.
  - One cache for the process rather than one per column family keeps its bound independent of the number of column families (18 × 32 MiB with the defaults). The index blocks go into it because the block store keeps
    every block and the state store every version from the pruning root on, so their table files, and the index blocks RocksDB would otherwise hold outside any cache, grow with the chain.
  - jemalloc rather than settings of glibc. In the three-node test with the variants side by side, from 4 000 to 16 500 blocks: glibc as it was went from 100 to 135 MiB, glibc with a fixed mmap threshold
    (`MALLOC_MMAP_THRESHOLD_=131072`) from 72 to 87–90 MiB, and glibc with two arenas (`MALLOC_ARENA_MAX=2`) from 81 to 100 MiB, both still rising. An environment variable would also reach only the processes
    whose launch we control (the devnet image), not an operator who runs the binary, and calling `malloc_trim` from the node needs unsafe code, which the workspace forbids. jemalloc returns freed pages to the
    system after a delay (10 s by default); it is the allocator RocksDB recommends, and Rust nodes on RocksDB commonly use it. mimalloc held 170–215 MiB in the same test.
  - No result depends on the allocator: consensus code reads neither addresses nor allocation order, and the determinism test (`ksana-execution`) does not link jemalloc.
  - A dependency outside the table of SPEC §12: `tikv-jemallocator` 0.7 (MIT / Apache-2.0) with `tikv-jemalloc-sys`, which builds jemalloc 5.3 (BSD-2-Clause) from source and needs `make` (present in the
    build image and on CI). jemalloc's page size is fixed when it is built, and a binary fails at startup on a kernel with larger pages: the build takes 64 KiB for aarch64 Linux, so that one binary runs on
    kernels with 4, 16 or 64 KiB pages (aarch64 kernels use all three), and elsewhere the page size of the build machine (4 KiB on x86_64, its only size). `JEMALLOC_SYS_WITH_LG_PAGE` sets another size at build time.
- **Impact**:
  - Measured after the change, with the same scratch test over 30 000 blocks: the write buffers of both databases held 18–27 MiB at every pruning, the block cache 8.4 MiB at the end, and the node's Rust heap
    12.7–12.9 MiB. The node binary under the same load, against the fake bitcoind of the node tests (transfers submitted through RPC, a block at most every 200 ms, a BTC block every two blocks, sampled
    every 500 blocks): `main`'s node grew from 95 MiB at 5 000 blocks to 151 MiB at 12 000, about 8 MiB per 1 000 blocks, and this change's from 86 to 98 MiB. Over 30 000 blocks the node with this change held
    87–101 MiB from 10 000 blocks on (92 MiB at 10 000, 97 MiB at 30 000): its write buffers moved within 17–27 MiB and its block cache reached 8.7 MiB of its 32.
  - With jemalloc, in the three-node test with glibc, jemalloc and mimalloc side by side (x86_64, 4 KiB pages): at 12 000 blocks glibc's node held 126.5 MiB and was still rising, jemalloc's had held
    85–98 MiB from 4 000 blocks on without a trend and used about 3% less CPU time, and mimalloc's held 170–215 MiB. Run again with two nodes of this change beside a node of the previous commit on glibc
    while a build kept the processors busy, so that settlements of the test's accelerated chain missed their timeout and the fast path paused now and then: from 4 500 to 10 000 blocks glibc's node rose
    from 115 to 147 MiB (its free memory from 46 to 74 MiB, its memory in use from 45 to 49) and the two on jemalloc from 101 to 117.5 and from 97.5 to 115 MiB. A third run over 16 000 blocks
    (beside a build until about 5 500): glibc's node went from 107 MiB at 4 000 blocks to 134 MiB at 12 000 and stayed there, jemalloc's moved within 94–110 MiB without a trend, and jemalloc with its
    background purge threads within 93–108 MiB, no different, so they stay off (jemalloc's default).
  - The 10-hour devnet soak of this change (ddf30c3, with `main` 5f77bba merged in) on the project owner's arm64 Mac (2026-10-12; three nodes and the indexer, 4 101 path B and 2 030 path C operations,
    7 308 blocks, four prunings per service): at every pruning the write buffers held 16–21 MiB in each node and 11–15 MiB in the indexer and the block cache under 1 MiB, and the data directories held
    about 58 MB per node and 45 MB for the indexer (about 157 MB in `main`'s soak). At the end the nodes' resident memory was 149, 140 and 130 MiB and the indexer's 67 MiB, against 192, 166 and 183 MiB
    and 96 MiB in `main`'s soak. After the first pruning the nodes' anonymous memory rose from about 95 to 112–121 MiB over about 2.5 hours and then levelled off: their resident memory stayed within
    124–149 MiB from about 6 hours on, while `main`'s kept rising. jemalloc there works with 64 KiB pages (the aarch64 default above); the runs on x86_64 did not show the rise after the first pruning.
  - Code: `ksana-state` (`rocks.rs`: `DbMemory`, `memtable_bytes`; `StateDb::open_with`, `StateDb::memtable_bytes`), `ksana-node` (`ChainStore::open`, one `DbMemory` for both databases, the log line, the global
    allocator in `main.rs`), `ksana-indexer` (`Index::open`, one `DbMemory` for the index and the state store and its rebuilds, the log line, the global allocator in `main.rs`); `tikv-jemallocator` in the
    workspace dependencies, for Linux only.
  - Tests: `ksana-state` `rocks::tests::write_buffers_stay_within_the_database_bound` (64 MiB written over three column families: the write buffers peak at about 23 MiB, within twice the bound, where the defaults
    hold 69 MiB; what was flushed reads back through the cache) and `the_databases_of_a_process_share_one_block_cache`; both fail with RocksDB's defaults.
  - Documentation: SPEC (version history, §10.2, §10.3, §16), CLAUDE.md, PROGRESS M25, this entry.

## D-104 A deep BTC reorg takes the blocks anchored on the orphaned BTC blocks off the main chain: every main-chain block above the finalized tip must be eligible (decided by the project owner 2026-10-08, `ANCHOR_DEPTH` kept at 2 on mainnet the same day; revises D-035, D-037, D-045, D-056, D-063, D-091, D-098, D-100, D-101, D-102 and SPEC §6.4, §6.5, §6.8, §7.3, §7.4, §7.6, §7.7, §10.3, §10.4, §11.1, §13.1, §15, §16, §17.3; fork choice, no change to block validity, the encodings or the finality determination, M29)

- **Problem**: D-101's known limitation. A block already on the main chain when a later BTC reorg of at least `ANCHOR_DEPTH` + 1 blocks (3 on mainnet, 2 on signet, 1 on devnet) orphans its anchor, or the anchor
  of a block it merges, can never become final, and neither can any block built on it (D-101 item c). The main-chain selection did not look at blocks already on the main chain again, so the producers kept
  building on such a block and every new block landed in its future: finality stopped for good at its parent, pruning stopped with it, and the kept state and memory grew with every block; a node synced after
  the reorg kept the block off its main chain while the older nodes stayed on it, and the indexers, whose header DAG did not look at anchors, followed the older nodes. Only a person could recover, by resyncing a
  producer, whose branch would then become final after `BTC_FINALITY_CONFS` confirmations and force everyone over (D-082). `m6_1_regtest` and the BTC-reorg tests of `indexer.rs` pinned the stall with a
  1-block reorg on devnet. A design for it was written and reviewed internally; the project
  owner, 2026-10-08: do it, and keep `ANCHOR_DEPTH` 2.
- **Decision**:
  a. **The rule** (§6.4): every main-chain block above the finalized tip must be eligible: not disqualified, and neither its anchor nor the anchor of a block it merges off the BTC best chain (the BTC block at that
     height is another one, the test of D-101 item c; an anchor above the BTC tip, or below the heights the snapshot holds, is not off it). When a BTC reorg makes a main-chain block above the finalized tip
     ineligible, the main chain retreats to the selected parent of the lowest such block, and the best candidate is then chosen as usual: each tip offers the highest block of its selected chain below its
     lowest ineligible block, and among those whose selected chain passes through the finalized tip the largest `(chain_work, hash)` wins. The retreat is forced: it is not bound by `REORG_WINDOW` and needs no
     more work, and since the retreated tip is a candidate itself, the worst case is a pure retreat. The main chain is thereby a function of the DAG, the BTC best chain, the finalized tip and the disqualified
     marks alone (in the node, `REORG_WINDOW` still bounds an ordinary switch, as before): an old node, a node synced after the reorg and an indexer on the same BTC chain choose the same main chain, the one
     the finality determination considers. When BTC reorgs back, the blocks come back to the main chain the same way, and the blocks built during the reorg, anchored on the branch it orphaned, leave it.
  b. **Node**: the check runs whenever the main chain is chosen (`Chain::update_virtual`), when the producer commits a block (`Chain::commit_block`), and once per BTC tip after a BTC sync that left the tracker caught
     up with bitcoind (`Chain::reselect`); it reads the BTC snapshot under the processing lock, so a block connected under a newer snapshot is never judged by an older one. `Chain::commit_block` checks the
     block it commits as well: when a BTC reorg synced after its template was built took its anchor, or the anchor of a block it merges, off the best chain, the block is discarded rather than committed and
     retreated from at the next check (found by the audit review of M29). The retreat takes the path of a main-chain switch (`Chain::rewind_to`), with an alert naming the block and the depth
     (`Chain::retreat_from_unanchored`); the state at the retreat target is at hand, since every version from the finalized tip on is kept (D-101). The path B ops of the blocks taken off come back to the entry
     pool as after any switch (D-041; after a BTC sync through `Chain::take_chain_changes`, D-082); a forced-inclusion entry comes back when its native transaction is on the new branch, since the window (§7.4)
     restarts from the retreated tip.
  c. **Header DAG and indexer**: every operation of `ksana_btc::headers::HeaderDag` that chooses the main chain takes the caller's BTC snapshot, and `HeaderDag::reselect` retreats the same way. The indexer calls it in
     every round after the finality determination and before planning, with an alert on a retreat; its execution rolls back to the fork point as for any main-chain switch, and every state version from the
     finalized tip on is kept already (D-101).
  d. **Competing chains** (D-091, local policy): `Chain::btc_competitor` takes from each tip's selected chain the part the finality determination can still finalize: below the lowest block that is anchored, or
     merges a block anchored, off the BTC best chain, or that the determination excluded. After a deep reorg the settlement transactions of the chain the nodes left confirm again on the new branch and put its
     headers back on BTC; that chain can never be final and must not make every funded node republish at the next-block rate, while the blocks below its first such block can still become final and race
     this node's chain like any lighter chain.
  e. **Funding inputs** (D-037, D-045, local policy): a commit / reveal pair this node broadcast for a block that has left the main chain (its own settlement, a rescue, a republish) keeps its funding inputs
     reserved while either of its transactions is in the BTC mempool, and releases them once neither is (`Chain::linger`, released in `Chain::process_settlements`). A BTC reorg puts the pairs of the blocks
     it orphaned back into the mempool, and the funding UTXOs are scanned from the confirmed chain only: spending such an input again conflicts with the waiting pair, and the producer had to discard a block it
     had solved. The reservation is kept in memory only; after a restart the producer's check against bitcoind discards such a block once and drops the conflicting UTXOs from its cache, as before.
  f. **CPFP** (D-102): a child this node sent that has left the BTC mempool is sent again only for a pair this node still settles; for the pair of a block that left the main chain it is given up, as when
     the pair no longer waits.
  g. **Unchanged**: the block validity rules, the encodings, the finality determination, the shared vectors and the golden roots. Nodes of different versions diverge after such a reorg until a branch without the
     block becomes final and forces the older ones over (D-082), so this change needs no new genesis by itself; devnet and signet restart from genesis once, together with M28 (D-101). `ANCHOR_DEPTH` stays 2 on
     mainnet, 1 on signet and 0 on devnet.
  h. **Entry pool** (D-098, local policy): the nonces path C ops waiting for forced inclusion take from the account's nonce on are no gap. The node passes the pool, by account in BTC order, the nonces of
     the native transactions on its BTC best chain above the inclusion height of its main-chain tip's anchor whose op passes the authentication of a forced entry, after every main-chain change and BTC sync
     (`Mempool::set_forced`, from `ksana_node::ops`), and admission, eviction and the re-check after a chain change start an account's consecutive run past those a block forcing them takes in turn: a block
     runs its forced entries before its path B ops, in BTC order. The packing candidates still start at the account nonce, so the path B ops above such an op follow it in the next block. A retreat returns
     the path B ops of every block it takes off at once, while the path C ops below them are included again only by the next blocks: counted as behind a nonce gap, those beyond `max_gapped_per_account` (8)
     were dropped and the ops signed above them refused. The 10-hour soak of M29 (2026-10-12, together with M30) stopped on it 64 minutes in: a 3-block reorg returned 8 path B ops above a path C op that
     the next block included again, and in between the nodes refused the op signed next, and the soak test's fill at its nonce, as a ninth op behind the gap. A nonce whose path C op is not waiting (it
     failed out of turn, or its native transaction is not on the BTC best chain: back in the BTC mempool, or double-spent) is a gap like any other, and the limit applies; so is the nonce of a path C op
     above a path B op of the account still in the pool, which the block forcing the path C op runs first, out of turn. The audit review of these changes (findings F1 to F3) made what the node passes follow what a block forcing the ops does. First, only an op passing the authentication of a forced
     entry counts: version, chain ID, encoded length, fee mode and the signature over the carrying transaction's first input, under the next block's rules (`ksana_execution::check_native_op`, which
     `Executor::forced_prepare` runs too), each transaction's verdict kept by txid. Recognizing a native transaction checks none of these, so anyone could have every pool count any account's nonce as filled
     while the transaction waits (about 40 minutes on mainnet), at the price of a BTC transaction and no KSA, and keep ops above it that stand as packing candidates and are never packed: what D-098 closed.
     Second, the nonces count in BTC order, each only when it is the account's next nonce after those before it, as a forced entry out of turn fails and takes none even when a later one fills the nonce
     below; counted as a set, two valid ops in the wrong order held slots the same way. Third, the scan covers the first `NATIVE_CONFIRMATIONS + ANCHOR_DEPTH + BTC_FINALITY_CONFS` BTC heights above that
     inclusion height, by reference: the heights the next blocks force first, after a retreat from a reorg as deep as BTC finality allows too, and leaving out the later ones only leaves their nonces gaps.
     Before, a node whose main chain was far behind BTC (offline for a while, or new and syncing a mature network from peers) copied the whole backlog of native transactions for every block it accepted.
  i. **Status** (D-063, local policy): the DAG tips a node carries in `Status` leave out those anchored, or merging a block anchored, off its BTC best chain (`Chain::status_tips`), such as the blocks
     a retreat leaves behind, and take them in again when BTC goes back to their branch. No block merging them could become final while that BTC chain stands, and a peer on the same BTC chain that
     lacks them refuses them for now (D-101 item c), so it fetched them again at every `Status` until they fell out of the merge depth, about 40 minutes later at the block rate of the soak that met
     it: in the local soak with BTC reorgs of M29 (2026-10-12, 80 minutes, 14 reorgs), two of the three nodes lacked blocks the others had taken just before they saw a reorg, and synced from them
     every few seconds throughout, 1 580 syncs that received 45 044 blocks and accepted 5, refusing the same blocks each time.
  j. **Orphans of a refused block** (D-100 item h, local policy): a block missing a parent whose refusal for itself (whatever the copy, and not because the local BTC view lags) is remembered under the
     current BTC tip is refused for now with it instead of being kept as an orphan, and this refusal is remembered for as long as the parent's (`Chain::refused_with_parent`), so the blocks built on it
     in a sync response are refused the same way; remembering such a refusal drops the orphans waiting for the block, and those waiting for them (`Chain::remember`). They come back with it in a later
     sync, once it can be taken. Item i left one way for the repeat to go on (the audit review of item i, finding N1): a sync
     through `GetBlocks`, started because a peer is ahead or has another unknown tip, still hands over the blocks a retreat left behind, parents first, and a node that refused one kept the block built
     on it as an orphan. It then fetched the refused block by hash at every change of that peer's `Status` and checked it again after every BTC block, for as long as that BTC chain stood, as the
     orphan pool has no expiry. A refusal because the local BTC view lags behind the block's anchor keeps the orphans waiting, as they are taken once the view catches up, and so does the refusal of
     one copy, as another copy can be taken.
  k. **Parallel blocks a retreat leaves behind** (D-063, local policy): a template on the main-chain tip tries, besides the DAG tips within the merge depth, the blocks below the tips anchored, or merging a
     block anchored, off the BTC best chain that are not, when all their children are and the main-chain tip has not merged them, by fork-choice key like the tips, and `Status` carries them in place of those
     tips (`frontier` in `chain.rs`, for `Chain::template` and `Chain::status_tips`). After a retreat, a parallel block that a block taken back had merged is such a block: it is no tip, the block taken back
     being its child, and yet nothing keeps it from being merged again. Left out, as it was, its path B ops came back through the entry pool behind the path C ops the next block includes again at its start,
     so a reorg of `ANCHOR_DEPTH` + 1 blocks (3 on mainnet), the shallowest that takes any block back, made such an op fail out of turn when the path B op below it was in the parallel block (the case the
     audit review of M29 found, Users below), and the parallel block's reward was lost. Such a reorg takes back only the block that merged it, and the retreat goes back to that block's selected
     parent, which is ahead of the parallel block by fork-choice key (it was chosen over it), so the next block can merge the parallel block again on the same terms, and its ops run ahead of that block's own
     entries, as they ran ahead of the path C op before the reorg. Failing out of turn then takes `ANCHOR_DEPTH` + 2 blocks (4 on mainnet) in both cases when the next block merges the parallel block again,
     as nodes do, and still `ANCHOR_DEPTH` + 1 when it does not: a node without item k, a producer at the merge limits, or a block built after the reorg on the block taken back, anchored on the new branch
     and merging nothing else: as its selected parent does not count (D-101 item c), it is no tip anchored off, so the walk never reaches the parallel block (only a producer that means to can build such a
     block, as an honest node's `commit_block` retreats first). Like any merged block, the parallel block holds the finality of the block merging it until its data lands: its own settlement transactions go
     out once that block reaches `SETTLEMENT_DEPTH`, those a BTC reorg took off confirm again on the new branch, and a rescue settlement carries the same data when they cannot (§7.3). It counts toward the
     settlement backlog limit too (D-046): when the reorg took its data off BTC and its anchor is `SETTLEMENT_TIMEOUT` or more below the merging block's (an hour of BTC blocks on mainnet), the blocks above
     it carry only forced-inclusion entries until that data lands; leaving such a block out of templates would bring back the out-of-turn failure. The walk goes down from the tips anchored off the BTC best
     chain through such blocks only, so it costs next to nothing while there is none. `Status` also carries the main-chain tip a retreat went back to when the blocks it took back were the tip's only
     children, where it carried no tip before, and a peer lacking the parallel block fetches it as it would a tip. The audit review found it sound. The project owner left the choice to the team on 2026-10-12; it went in after the rest of M29, as a change of
     its own.
- **Rationale**:
  - Such a block could never become final under D-101, so it was going to be given up anyway; the old behavior only waited for a person. The retreat makes it happen at once, automatically and alike on every
    node and indexer.
  - **D-035 and D-056 revised**: a BTC reorg leaves the fast lane unchanged (D-035, §15) only when it is at most `ANCHOR_DEPTH` blocks deep, so that its fork point is at or above every anchor. A deeper reorg
    with fork point F takes back the blocks anchored on F + 1 and above, and the blocks built on them. They are not invalid (D-035's "accepted blocks stay valid" stands): they are off the main chain while this BTC chain holds, and
    come back if BTC reorgs back. With anchor depth d, producers begin anchoring on F + 1 when the BTC tip reaches F + 1 + d, so a k-block reorg takes back the blocks produced during about k − d BTC block
    intervals: on mainnet about 10 minutes for k = 3 (about 120 blocks when busy) and about 40 minutes for k = 6. A reorg of at most `BTC_FINALITY_CONFS` blocks never takes back a final block (the data of a block
    anchored on F + 1 lands at F + 2 at the earliest and is final at F + 7 at the earliest, above the old branch's top F + k); a deeper reorg revokes the finality decisions first (D-082), as before.
  - **Users** (§10.4): a path B op of a block taken off comes back with the same op ID and is included again at once; reversing a payment in a natural deep reorg needs the sender to get another op with the
    same nonce included first. A miner who can make the reorg need not race: knowing the fork point, it can prepare a fast-lane branch on the retreat target, anchored on its own withheld BTC blocks, holding
    the double spend; when the reorg is published, the nodes retreat and choose the heaviest eligible branch, its own. Against such a miner a fast confirmation is worth only its anchor's BTC confirmations:
    reversing a block m BTC blocks after it was produced takes a reorg of at least d + 1 + m blocks. The same attack worked before this change (its branch was the only one that could become final, and
    finality then forced every node over, D-082), only an hour or so later; a miner who can do it can double-spend BTC itself. A path C op is included again when its native transaction is on the new branch,
    and is gone, as on BTC, when the native transaction was double-spent; one dropped from the mempool (the soak test met bitcoind dropping one that spent a coinbase output the reorg made immature) is
    included once it confirms again. Unlike on BTC, being included again can change its outcome: it runs in the new branch's order, which BTC keeps only for transactions spending one another, and at the
    start of its block, ahead of the path B ops of the blocks taken off, which come back through the entry pool; an op whose nonce is then out of turn fails without consuming it (§7.4), and the account's ops
    above it wait in the entry pool until the account fills the gap, among them any op its wallet signed on the nonces of the chain the nodes left. Failing out of turn takes a reorg that takes back the
    blocks holding both ops, whether or not it moves the native transaction: at least `ANCHOR_DEPTH` + 2 blocks (4 on mainnet) along one chain of blocks (the path C op's block anchors above the path B op's,
    since its window holds a native transaction its parent's did not); when the path B op is in a parallel block merged by the block that included the path C op, from `ANCHOR_DEPTH` + 2 blocks when the next
    block merges the parallel block again, as nodes do (local policy, item k), and from `ANCHOR_DEPTH` + 1 (3 on mainnet) when it does not. `ANCHOR_DEPTH` + 1, the shallowest reorg that takes any block back,
    takes back only the block that merged it; the parallel block stays eligible, and the next block merges it again, its ops ahead of that block's own entries. Until item k it was enough: the parallel block
    is no tip (the block taken back is still its child), so the new branch did not merge it again, and its ops came back through the entry pool too. The audit review of M29 found both depths
    (the earlier text gave `ANCHOR_DEPTH` + `NATIVE_CONFIRMATIONS`, the depth that moves an included native transaction), and `deep_reorg.rs` pins
    them with mainnet's parameters. The soak test met it on devnet, where it takes 2 blocks (1 with the path B op in a merged parallel block before item k), after 8 of 14 reorgs of 2 or 3 blocks over five
    runs, and after none of 11 of 1 block (PROGRESS M29). The user resubmits, as before for a native transaction the window had already passed (§17.3). Executing such ops in nonce order instead would need
    new state and inclusion rules. The ops that come back meet the entry pool's local policy like any others (D-098): behind a nonce gap the pool keeps at most `max_gapped_per_account` (8) of an account's
    ops and drops the last ones, which the wallet signs again. The nonces of path C ops waiting to be included again are no gap from the account's nonce on (item h); until they were, the soak test met the
    pool dropping the op it had just signed when a 2-block reorg brought back 8 of its account's path B ops above a path C op, and the Mac's soak met the nodes refusing one when a 3-block reorg did. A nonce
    a path C op leaves open, failing out of turn or with its native transaction off the BTC best chain, is a gap.
  - **Two limitations of §17.3 disappear**: native transactions after a BTC reorg (D-035, D-056 item (2)): the block that included a native transaction a reorg moved is anchored at or above the native
    transaction's old height, above the fork point, so it leaves the main chain and the window restarts below the new height (what remains is the order it then runs in, under Users, a narrower row of
    §17.3); and a deep reorg changing what executing an accepted block again finds (D-100):
    everything a block's execution reads lies at or below its anchor height, so a reorg that changes it orphans the anchor, and the block leaves the main chain instead of being executed again.
  - **What triggers it**: only a reorg of at least `ANCHOR_DEPTH` + 1 blocks on this node's own bitcoind (checked in the review of the design). A bitcoind that lags or is still in its initial sync does not trigger
    it, since a height it does not have yet holds no different block; every anchor a node accepts lags its BTC tip by at least `ANCHOR_DEPTH` (`Chain::check_anchor`, for new blocks and, since D-101 item i, for
    historical ones); the forged and stale anchors of D-101's second review stay side blocks under D-101 item i and are not touched. An operator's `invalidateblock`, or a bitcoind eclipsed or fed a heavier
    private chain, triggers it locally; that takes real work, and the node recovers once its view converges.
  - **`ANCHOR_DEPTH`** (the project owner kept mainnet at 2): the recorded natural reorgs of the BTC mainnet are 1 or 2 blocks deep (2 blocks at height 941 880 on 2026-03-23, an ordinary race); every deeper one
    came from a consensus failure (July 2015, 6 and 3 blocks of SPV mining that upgraded nodes never followed; BIP50 in March 2013, about 24 blocks; the value overflow of August 2010, 53 blocks). With this change a
    reorg of 3 or more blocks costs an automatic, bounded retreat, while a depth of 3 would make every path that reads BTC (forced inclusion, contracts reading headers, unlocking) about 10 minutes slower for an
    event not seen naturally. signet keeps 1, so that 2-block reorgs exercise the retreat (the public signet once had a 19-block fork, at height 287 766); devnet keeps 0.
  - **Alternatives not taken**: the status quo with manual recovery (a person on call, the state growing meanwhile); a BTC view frozen at the determination height (rejected by D-101 for the code it needs on a
    consensus path, and the older nodes would keep the blocks anyway, so it would need this change too); letting nodes synced after the reorg put such blocks on the main chain (reopens the stalls of D-101 item
    i); only raising `ANCHOR_DEPTH` (any finite depth has a reorg that triggers the stall).
- **Impact**:
  - Fork choice only; local policy for items d to f and h to k. Code: `ksana-btc` (`headers.rs`: the BTC snapshot on every main-chain choice, `HeaderDag::unanchored`, `HeaderDag::reselect`;
    `MockBtc::reconnect` for a BTC reorg back to a branch; `BtcSnapshot::native_iter`), `ksana-node` (`chain.rs`: `retreat_from_unanchored`, `reselect`, the checks in `commit_block`, the
    competing-chain part, `status_tips`, `frontier` for templates and `Status`, `refused_with_parent` and the orphans dropped with a refusal, `linger` and the lingering inputs; `rescue.rs`: the pairs of
    blocks taken off linger, the CPFP give-up; `ops.rs`: the nonces of the path C ops waiting for forced inclusion, with the verdicts on their ops kept in `Chain`), `ksana-mempool` (`Mempool::set_forced`),
    `ksana-execution` (`check_native_op`, the authentication of a forced entry), `ksana-indexer` (the reselect after the finality determination, the alert).
  - Tests: unit tests of the header DAG (a reorged anchor takes the main chain back, so does one of a merged block, and a BTC view without that height restores it; the reselect starts above the floor;
    a property test: the same headers in any order, each under any of four BTC views with reselects anywhere, end on the same main chain under the final view); the reorg sweep
    `crates/ksana-indexer/tests/deep_reorg.rs` (anchor depth 0 to 3, reorgs of 1 to d + 2 blocks and of 6 and 7, with empty blocks, a path B op, a path C op whose native transaction confirms again or
    is double-spent, settlement pairs waiting in the mempool or landed on the old branch, and BTC going back to the old branch; each checks the retreat target, that no block that was final leaves the
    main chain for k ≤ `BTC_FINALITY_CONFS`, that no main-chain block above the finalized tip is anchored off the BTC chain, that finality passes the retreat, the balances, the indexer height by
    height and a node synced from genesis afterwards; `cargo test` runs the boundaries and twelve random cases with extra fast-lane blocks and a sync between the reorg and the new branch, and the full
    sweep is `--ignored`; also a retreat into a full entry pool, a producer that sees the reorg late, a path C op that, included again ahead of the path B op below it, fails out of turn while the ops
    above it, one back in the entry pool and one signed on the old chain, wait for the gap to be filled, how deep a reorg must be for that with mainnet's depths (`ANCHOR_DEPTH` + 2 blocks, where one block
    less is not enough, along one chain of blocks and with the path B op in a merged parallel block, which the next block merges again after a reorg of one block less, item k), a retreat returning more path
    B ops above a path C op waiting to be included again than the entry pool keeps behind a gap (all kept, and the op signed above them admitted), only the path C ops a block forcing them takes counting in
    the entry pool, in BTC order (a forged signature, and two ops in the wrong order), and only from the heights the next blocks force, a block a retreat left behind not carried in `Status` until BTC goes
    back to its branch, the main-chain tip the retreat went back to carried instead, the blocks built on a block refused for now not kept as orphans (refused with it, from memory the second time, and orphans
    that came first dropped with the orphans waiting for them, until BTC goes back to their branch), and the funding inputs of a waiting pair staying reserved and released once it confirms); two entry pool
    tests (the nonces of path C ops waiting for inclusion are no gaps from the account's nonce on, in admission, eviction and the re-check, and are gaps again once the node no longer passes them; one above a
    path B op still in the pool is a gap; they count in BTC order); node tests (a chain anchored off the BTC best chain is no competing chain, while the blocks below such a block still compete and are raced;
    pruning goes on after a retreat; the CPFP child of a pair whose block left the main chain is not sent again; a block whose anchor a BTC reorg synced while it was produced took off the best chain is not
    committed; the orphan of a block refused because the local BTC view lags, or of a refused copy, is kept and taken with it); `m6_1_regtest` (on bitcoind, a path B op moved by a 1-block reorg and a path C
    op by a 2-block one). Changed to the new behavior: `m6_1_anchor`, `adversarial_liveness`, three BTC-reorg tests of the indexer, `m6_1_regtest`, and with item k `m18_rebuild` (the next block merges the
    block below those anchored on the BTC blocks a reorg orphaned, and finality passes it once the data of that block's chain, which the reorg took off BTC, lands again). `devnet/soak.sh` gains
    `KSANA_SOAK_REORGS_PER_HOUR`: random BTC reorgs of 1 to 3 blocks, after each of which finality must pass the fast-lane tip of that moment on the three nodes and the indexer within 15 minutes, and an op a
    reorg leaves behind a nonce gap must be included once the soak test fills the gap with path B ops, or its nonce filled too when the entry pool dropped or refused it (a refused fill is tried again); the
    soak test signs each op on the highest nonce of the three nodes.
  - Documentation: SPEC (version history, §6.2, §6.4, §6.5, §6.8, §7.3, §7.4, §7.6, §7.7, §10.3, §10.4, §11.1, §13.1, §14, §15, §16, §17.3, §17.4), PROGRESS M29, CLAUDE.md, notes in D-035, D-056,
    D-063, D-091, D-098, D-100 and D-101, this entry.

## D-105 Halving periods that double: about 2, 4 and then 8 years, at 4.2 KSA per slot (decided by the project owner 2026-10-12; revises D-074, D-089, D-095 and SPEC §8.6, §13, §13.1, §14; consensus change for mainnet only, M30)

- **Problem**: D-089 halves the mainnet reward every 12 500 000 DAA score (about two years): 75% of the supply is issued in the first 4 years, and in year 12 a slot pays 0.13 KSA (1/64 of the start).
  The project owner, 2026-10-12: "I want to set the halving periods to 1/2/4/8, or 2/4/8 ... what do you think". Ksana's security is paid for in BTC: every block's header and body are landed on BTC by
  someone who pays the BTC fee (settlement, republish, rescue; D-077, D-091, D-095), and that cost does not halve with the subsidy, while producers and carriers are paid mostly in KSA subsidy until a fee
  market exists (the fee competition review of 2026-10-05 found the subsidy dominating producers' income; finding 3 of the path assessment of 2026-10-07: landing costs BTC and earns KSA, and when nobody
  keeps paying to land, honest data stops landing in time). For the same BTC cost, the KSA price at which landing pays for itself is inversely proportional to the subsidy per slot.
- **Options considered** (exact sums in base units; "years" at 5 seconds per DAA score unit):
  1. Doubling forever (periods of 1, 2, 4, 8, 16, … years): every period issues the same amount (the reward halves as the length doubles), so there is no max supply: it grows with the logarithm of time.
  2. Doubling under the cap of 210 million: the capped payout (D-074) cuts the subsidy to 0 within one block when the cap is reached (1/2/4/8 years at 8.4 KSA fills it in year 14.9, falling from 1.05 KSA
     per slot to nothing), a cliff harsher than any halving.
  3. Doubling a fixed number of times, every later period keeping the last length: the tail is a converging geometric series, bounded by `(doublings + 2) × BLOCK_REWARD × REWARD_HALVING_INTERVAL`.
     Periods of 1/2/4/8 years (3 doublings from 6 250 000 at 6.72 KSA) and of 2/4/8 years (2 doublings from 12 500 000 at 4.2 KSA) both fill the cap exactly; after a first year that issues 20% of the
     supply, 1/2/4/8 is 2/4/8 delayed by one year and scaled to 80%, so the two differ only in how much the first year issues (20.1% against 12.6%).
- **Decision** (the project owner chose option 3 with 2/4/8 years, then every 8 years, on 2026-10-12 06:29 UTC, from an analysis of the options):
  1. **Rule**: a new per-network consensus parameter `REWARD_INTERVAL_DOUBLINGS`. Halving period k (k = 0, 1, …) lasts `REWARD_HALVING_INTERVAL × 2^min(k, REWARD_INTERVAL_DOUBLINGS)` DAA score units and
     every reward slot in it pays `BLOCK_REWARD >> k` (0 from a shift of 128 on); with 0 doublings this is D-074's `BLOCK_REWARD >> ⌊d / REWARD_HALVING_INTERVAL⌋`. The upper bound of total rewards becomes
     `max_total_rewards = (REWARD_INTERVAL_DOUBLINGS + 2) × BLOCK_REWARD × REWARD_HALVING_INTERVAL` (each doubling period pays at most `BLOCK_REWARD × REWARD_HALVING_INTERVAL`, the periods of equal length
     after them at most twice that together), and with it the limit on the genesis allocation. Like the other issuance parameters it is fixed per network, not in `Rules` or the upgrade schedule.
  2. **Mainnet**: `BLOCK_REWARD` 4.2 KSA (420 000 000), `REWARD_HALVING_INTERVAL` 12 500 000, `REWARD_INTERVAL_DOUBLINGS` 2: periods of 12 500 000, 25 000 000 and then 50 000 000 DAA score (about 2, 4 and
     then 8 years), so the reward halves in about years 2, 6, 14, 22, 30, …; the bound is 4 × 4.2 × 12 500 000 = 210 million KSA, so there is still no genesis allocation. The exact sum is 209 999 996 KSA:
     from the third period on, the periods are all 50 000 000 long, and at each position their rewards sum to 2 × 105 000 000 − 8 base units (105 000 000 has 8 one-bits; the first shift that
     truncates is in period 9), which over the 50 000 000 positions is 4 KSA. The last base unit is paid in period 28; from DAA score 1 387 500 000 (about year 220) the reward is 0.
  3. **Block share** unchanged: its fraction and its end in DAA score stay as D-089 set them [not published, D-134]. The carrier reward (D-095) is unchanged in its ratios: 5% of each slot, 0.21 KSA in the first period,
     about 25 KSA per BTC block (D-095's 0.42 and 50 were D-089's first period).
  4. **devnet / signet**: `REWARD_INTERVAL_DOUBLINGS` 0, so their schedule, bound and genesis allocation limit stay exactly as they were, and this decision needs no restart of either network.
  5. **Emission schedule** (mainnet; the carrier column is an upper bound, as without a carrier its part goes to the producer):

     | Halving period | DAA score | Roughly | Reward per slot | Carrier / producer and block share | Issued in period | Cumulative (of supply) |
     | --- | --- | --- | --- | --- | --- | --- |
     | 0 | 0 – 12 499 999 | years 0–2 | 4.2 KSA | 0.21 / 3.99 | 52.5 million | 52.5 million (25%) |
     | 1 | 12 500 000 – 37 499 999 | years 2–6 | 2.1 KSA | 0.105 / 1.995 | 52.5 million | 105 million (50%) |
     | 2 | 37 500 000 – 87 499 999 | years 6–14 | 1.05 KSA | 0.0525 / 0.9975 | 52.5 million | 157.5 million (75%) |
     | 3 | 87 500 000 – 137 499 999 | years 14–22 | 0.525 KSA | 0.02625 / 0.49875 | 26.25 million | 183.75 million (87.5%) |
     | 4 | 137 500 000 – 187 499 999 | years 22–30 | 0.2625 KSA | 0.013125 / 0.249375 | 13.125 million | 196.875 million (93.75%) |
     | 5 | 187 500 000 – 237 499 999 | years 30–38 | 0.13125 KSA | 0.0065625 / 0.1246875 | 6.5625 million | 203.4375 million (96.875%) |
     | 6 | 237 500 000 – 287 499 999 | years 38–46 | 0.065625 KSA | 0.00328125 / 0.06234375 | 3.28125 million | 206.71875 million (98.4375%) |
     | 7–28 | from 287 500 000 | years 46–220 | halving each period down to 1 base unit | 5% / 95% | 3.281246 million in total | 209 999 996 KSA (≈ 100%) |

     How the last part divides between the block share account and the producer [not published, D-134].

     Against D-089: the first year issues 12.6% of the supply instead of 25.2%; half the supply is reached in year 5.9 instead of year 2, 90% in year 25 instead of year 6.7; the yearly inflation is
     about 5.6% in year 8 (1.7%), 4.6% in year 12 (0.4%) and 1.9% in year 20 (0.02%); a slot pays 8 times as much in year 12, 16 times in year 16 and 64 times in year 20.
- **Rationale**:
  - The subsidy that pays for landing data on BTC lasts decades longer, while the BTC cost of landing never halves. D-095 put the break-even KSA price of the carrier's fixed part at 0.05–0.15 USD (10
    sat/vB, D-089's first period); with D-089 it would be 3.2–9.6 USD in year 12, with this schedule 0.4–1.2 USD (and 0.10–0.30 USD in the first period, whose slot pays half of D-089's).
  - Fixing the length after the third period keeps the max supply of 210 million without the cliff of option 2, and with a tail that converges rather than option 1's endless issuance.
  - 2/4/8 rather than 1/2/4/8: the first year is when the hashrate is lowest (at the PoW limit about 13 CPU cores make one block per 5 seconds, §13.1) and the fewest people take part, so issuing 20% of the
    supply then, with the block share on top, concentrates it in a few early hands. 1/2/4/8 would give more incentive at the cold start and an earlier first halving.
  - One parameter that generalizes the rule, rather than mainnet's periods written into the code: devnet / signet keep their schedule through the same code path, which is tested with any number of doublings.
- **Costs**: early producers get less (37.7% of the supply in the first 4 years instead of 75%; 4.2 instead of 8.4 KSA per slot at the start); inflation is higher in the middle years; the block share's total halves with
  the reward over its unchanged end (the project owner kept the share as it is); "halving every two years" no longer describes the schedule.
- **Impact**: a consensus change for mainnet only, made before its launch, so it takes effect with the mainnet genesis; devnet and signet keep their behavior (0 doublings) and need no restart; the encodings,
  the shared vectors and the golden state roots are unchanged (`determinism.rs` has no unlock entries).
  - `ksana-primitives::params`: `Params::reward_interval_doublings`, `reward_period_at` (a walk over the doubling periods, at most 65 rounds for any parameters), `block_reward_at` by period,
    `max_total_rewards` with the new bound; `MAINNET` 420 000 000 / 12 500 000 / 2; `DEVNET` / `SIGNET` 0.
  - TS SDK: `Params.reward_interval_doublings`, `rewardPeriodAt`, `blockRewardAt` by period, `maxTotalRewards` (new, the same bound); `MAINNET` likewise.
  - Web page: the `REWARD_INTERVAL_DOUBLINGS` row of the parameter table (devnet 0) in every language, and the simulator's reward slots by period.
  - Tests: `params.rs` (mainnet's periods, boundaries, splits, share total and exact sum; doubling periods; any number of doublings neither loops long nor overflows), `reward_props.rs` (the period found by a
    walk over the period lengths, the sum over all slots against the period-by-period total and the bound, the existing properties with doublings), `rewards.rs` (unlocking main-chain blocks across doubling
    periods, and an empty block whose slots cross two period boundaries), `params.test.ts`.
  - Documentation: SPEC version history, §8.6 (reward schedule, block share and carrier figures, emission table), §13, §13.1, §14; the figures of items 2–4 and 7 of D-089 and the per-slot figures of D-095
    are revised by this entry; `docs/PENDING_REVIEW.md` points here; PROGRESS M30, CLAUDE.md.

## D-106 Clients take the network from the node: the CLI reads the node's chain ID, the TS SDK has no default network, the web page checks the chain ID, its Bitcoin RPC's network and the wallet's (the second internal audit's A2-11, 2026-10-13; revises D-051, D-053, D-055, D-096 and SPEC §11.3, §11.4, §16; client side only, no consensus change, M31)

- **Problem**: the second internal audit's A2-11 (`audits/2026-10-06-second-internal-audit.md`), to fix before mainnet: the CLI's `--chain` defaulted to devnet and nothing called
  `ksana_getChainInfo`; the TS SDK's `buildTopUp`, `buildNativeTx`, `nativePayload` and the payload codec defaulted `params` to `DEVNET`; the web page took the network chosen on the page for the
  node's (anything but signet as devnet) without asking the node. A buyer who runs `top-up` against a mainnet or signet node without `--chain` signs the claim op with devnet's chain ID; the KSN1 magic of
  the data output is the same on every network, so the node force-includes the native transaction, the op fails on its chain ID, and the payment to the seller's script confirms anyway. Any path C op
  fails the same way. Decoding a payload with another network's parameters silently gives its VmOps that network's chain ID, since the compact entries leave it out (D-080; the first audit's F-34).
- **Decision**:
  1. **CLI**: the network is the node's. Every command but `key` first reads the node's chain ID (`ksana_getChainInfo`) and takes the network from it (`Network::from_chain_id`), refusing a chain ID of no
     network; the VmOp chain ID, the BTC network of account addresses and that of `exchange lock --address` follow it. `--chain` no longer has a default: given, it states the network expected, and a
     node on another network is refused before anything is read, signed or sent. `key` does not talk to the node, so there `--chain` only picks the address network (devnet when absent, as before).
     Path C and top-up also compare bitcoind's `getblockchaininfo` `chain` with the BTC network of the node's chain (`regtest`, `signet`, `main`) before anything is funded or signed: the bitcoind wallet
     funds and broadcasts the transaction, and on another BTC network the payment is real (for a top-up it goes to the seller's script, which is the same on every network) while the node never sees it.
  2. **TS SDK**: nothing defaults to a network. `params` is required by `buildNativeTx`, `buildTopUp`, `nativePayload`, `nativeDataOutput`, `txPayload`, `nativeOpFromTx`, `encodePayload`,
     `decodePayload` and `decodeVmOp` (the types say so, and a JavaScript caller that leaves it out of `buildNativeTx`, `buildTopUp` or `nativePayload` gets a `TypeError` naming
     `KsanaRpcClient.getParams`); the generic `encode` / `decode` no longer take `'Payload'`, whose encoding depends on the network. New: `NETWORKS`, `networkOfChainId`,
     `paramsForChainId(chainId, expected?)` and `KsanaRpcClient.getParams(expected?)`, which pick the parameters by the node's chain ID and throw `ChainMismatchError` for a chain ID of no network or of
     another network than `expected`. `nativePayload`, and so `buildNativeTx`, throws it for an op whose chain ID is not that of the parameters; `btcNetworkOf` throws it for a chain ID of no network
     instead of answering regtest.
  3. **Web page**: the node's chain ID must be that of the network chosen on the page. It is read when the connection is tested, before the first sync shows anything and again before the wallet signs
     anything (path B, path C, top-up). A node on the other network is refused with what to do and, on the connecting screen, a button that switches the page to the node's network; a node on mainnet is
     refused, since the page does not support mainnet yet (it has no mainnet BTC network, parameter table or simulator, and nothing should be signed for mainnet from it before mainnet launches); a chain ID
     of no network is refused as a wrong address. The Verify page's parameter table follows the network (signet: chain ID 1002, `ANCHOR_DEPTH` 1). Its Bitcoin RPC must be on that network's BTC
     network too, as the CLI's bitcoind is (added on the audit review of the fix): the page lists the wallet's UTXOs there with a
     `scantxoutset` `raw()` descriptor, which matches the same script on every network, and broadcasts there, so `getblockchaininfo`'s `chain` (`regtest` for devnet, `signet`) is read before every
     listing of coins, every broadcast and before path C or a top-up is signed, and another chain is refused with what to do (point the Bitcoin RPC at a bitcoind on the right network, or clear it
     and enter the UTXOs by hand). So must the wallet be, as far as it tells (an optional item of the audit's review of that increment, same file, done in a follow-up): the address pins the BTC
     network on devnet (`bcrt1`) but not on signet, whose `tb1` is testnet3's and testnet4's too (nor on mainnet, whose `bc1` is Fractal Bitcoin's too), and without a Bitcoin RPC the page used
     Unisat's own coin listing (`getBitcoinUtxos`) and broadcast (`pushTx`), which work on the chain Unisat is set to: a Unisat on testnet4 had its testnet coins listed and spent there, where the node
     never looks. The SDK's adapters now say which BTC network the wallet is set to (`WalletAdapter.getNetwork`: Unisat `getChain`, Xverse `wallet_getNetwork`; `undefined` when the wallet cannot
     tell). The page refuses a wallet that names another network than the page's when it connects and before path C or a top-up is signed, and uses Unisat's own listing and broadcast only for the
     connected Unisat wallet once it names the page's network (they used to serve whichever wallet was connected, listing Unisat's account for an Xverse or test-key wallet). A wallet that cannot tell
     is left to the address check; without a Bitcoin RPC its coins are then entered by hand and the signed transaction is handed back, as for a wallet without those services.
- **Rationale**:
  - The node is the party the op is sent to, so its chain ID is exactly the one the op must carry. Taking the network from it, rather than making `--chain` mandatory, removes the failure for anyone who
    forgets the flag and keeps existing scripts and instructions working (`docs/SIGNET.md` passes `--chain signet`, which now also guards against a node on another network); `--chain` stays for those who
    want to be sure which network they sign for.
  - A node that lies about its chain ID can make a client sign for the wrong network, but such a node can already lie about the lock, the account or the fee; trusting the node is the first audit's
    open F-15, not this finding.
  - The SDK is a library and does not know which node its caller talks to, so it makes the caller name the network and offers the helpers that get it from the node.
  - The web page refuses mainnet rather than half-supporting it: until mainnet launches, a mainnet node behind the page is a mistake.
  - The wallet's network matters where the page relies on the wallet's own BTC services, Unisat's listing and broadcast: the signatures do not depend on the network, and a checked Bitcoin RPC
    lists the right coins and broadcasts on the right chain. A wallet that names another network is refused anyway, so that the wallet and the page agree on what the user sees; one that cannot
    tell is not, since that would shut out older wallets for no gain in safety.
- **Costs**: every CLI command that talks to the node makes one more RPC call, and path C and top-up one more bitcoind call. SDK callers must pass the parameters (a breaking change of the SDK's API,
  which is private and versioned with the repository). The web page cannot be pointed at a mainnet node until it supports mainnet. The page's Bitcoin RPC gets one more call before each listing of
  coins and each broadcast, and a proxy in front of it that allows only some methods must let `getblockchaininfo` through; its `chain` does not tell the default signet from a custom one (D-108's
  limitations). The wallet is asked for its network (no prompt) when it connects, before path C or a top-up is signed and before Unisat lists coins or broadcasts; a Unisat version without
  `getChain` no longer lists coins or broadcasts for the page.
- **Impact**: client side only: no change to consensus, the encodings, the parameters, the vectors, the RPC or the node; no restart.
  - `ksana-cli`: `Node::connect` / `node_network`, the commands that talk to the node split into `NodeCommand`, `btc_rpc` / `check_btc_chain`. Tests: the unit tests `the_network_is_the_nodes` and
    `bitcoind_on_another_btc_network_is_refused`, and `tests/chain.rs`, which runs the binary against a mock node served by `ksana_rpc::start_server` and a fake bitcoind (a node on another network than
    `--chain`, or on none, is refused with nothing submitted; ops are signed for the node's network and addresses parsed for it; path C and top-up refuse a bitcoind on another BTC network before
    anything is funded, and go on to funding with one on the right network).
  - TS SDK: `params.ts`, `codec.ts`, `pathC.ts`, `topUp.ts`, `rpc.ts`, `btc/address.ts`; `test/network.test.ts`, and the existing tests pass `DEVNET` explicitly; the regtest end-to-end script and
    the wallet demo take the parameters from the node.
  - Web page: `RpcChain.checkChain`, the data source panel, the connecting screen, the send and top-up pages, the messages in every language, the Verify page; `chain.test.ts` (a node on signet,
    mainnet or no network refused before anything else is read; the chain ID read again before signing). The Bitcoin RPC: `BtcRpc.checkChain`, `BtcChainMismatchError`, `RpcChain.checkBtcChain`;
    `wallet/btc.test.ts` (a Bitcoin RPC on another BTC network refused before coins are listed or anything is broadcast; path C and top-ups refused before the wallet is asked to sign, for a node or
    a Bitcoin RPC on another network). The wallet's network: in the TS SDK `WalletAdapter.getNetwork` / `WalletNetwork`, `UnisatProvider.getChain` / `UnisatChain`, `XverseAdapter.getNetwork`,
    the mock wallet's `getChain` and `chain` option (`test/wallet.test.ts`); in the page `checkWalletNetwork`, `WalletNetworkMismatchError`, `connectWallet`, Unisat's listing and broadcast for the
    connected Unisat wallet only, the message in every language (`wallet/btc.test.ts`: a wallet set to another network refused when it connects, before coins are listed and before the wallet is
    asked to sign; Unisat on the page's network lists and broadcasts; one that cannot tell, or a wallet other than Unisat, gets neither).
  - Documentation: SPEC version history, §11.3, §11.4, §16; the second audit (A2-11, and the first audit's F-22 and F-34 in its status table); `docs/WEB_DESIGN.md` §7, `web/README.md`,
    `sdk/ts/README.md`, the wallet demo's README; PROGRESS M31, CLAUDE.md.

## D-107 The public signet: Ksana on Bitcoin's public signet, hosted on one server deployed over ssh, with a relay probe, a KSA faucet and a BTC proxy for the web wallet (the project owner chose the public signet on 2026-10-13; revises SPEC §12, §14, `docs/SIGNET.md` and, on the hosted page, D-108's scan fallback; deployment and documentation, no consensus change, M32)

- **Problem**: the last step before mainnet is a test network that others can join: real wallets on the public signet and at least four weeks of observing `SETTLEMENT_TIMEOUT` and the carrier
  reward on a real fee market (SPEC §14). Until now signet existed only as parameters (`SIGNET`, chain ID 1002, `ANCHOR_DEPTH` 1, D-055, D-056) and the M6 manual for one private node: no genesis config,
  no seed node, nothing a visitor could open. Two questions came first: whether Bitcoin's public signet carries Ksana's data at all (its blocks are signed by a few miners, who could filter the
  envelope in a reveal transaction's witness, D-081, or path C's large `OP_RETURN`, D-032), and how to host the network on one rented server (Ubuntu 22.04 or 24.04; the first one has 4 cores, 8 GB of memory and
  about 110 GB of free disk) from the project owner's Mac without handing anyone its keys.
- **Decision**:
  1. **The public signet** (the project owner, 2026-10-13: "Should we use the public signet or a self-hosted one? I suggest the public one: it covers a wider range of situations."). Before the network
     starts, the relay probe checks that signet's miners mine both carriers; if they do not, moving to a self-hosted signet (its own `-signetchallenge`) is the project owner's decision.
     On 2026-10-13 they mined both: all five of the probe's tests in the next block (PROGRESS M32).
  2. **Relay probe** (`crates/ksana-btc/examples/relay_probe.rs`, built into the image as `ksana-relay-probe`): from one key, it sends commit / reveal pairs built exactly like settlement transactions
     with 600-byte and 81 920-byte (`MAX_BLOCK_DATA_BYTES`) envelopes, and transactions with 220, 4 096 and 65 536-byte (`MAX_OP_BYTES`) KSN1 `OP_RETURN` outputs, checks each with the local bitcoind
     first, follows the chain and prints where each was mined; exit status 0 only when all were. The payloads start with `0xff`, which is no payload kind, so no node or indexer reads them.
     It pays bitcoind's estimate for confirmation within 2 blocks, at least 2 sat/vB, plus 1 (`--feerate auto`, the default), because a rate the miners do not take at the moment looks the same
     as a filter. It first splits its funds into one output per test and waits for that split to confirm; a split that waits is replaced by running the probe again with `--replace <txid>`, which
     spends the split's inputs again at the current rate (every input signals replacement, BIP125), because the probe's watch-only wallet (D-109) does not list the coins a waiting transaction spends.
     At the split's own rate the same tests give the same transaction again (its signatures are deterministic), which bitcoind takes as the one already waiting, so nothing would be replaced:
     the probe refuses that and asks for a higher `--feerate` (the fourth audit round of #35).
  3. **Hosted stack** (`signet/`): one Docker Compose project, its images built on the server from the committed tree (`signet/Dockerfile`): bitcoind (Bitcoin Core 31.1, the official image pinned by
     digest, `-signet -txindex`, RPC by `-rpcauth` with a random password), the seed node `node1` (produces and settles, public P2P port 9000; `node2` as a second producer only on a server with 4 cores
     or more), the indexer, a BTC proxy and a KSA faucet (`signet/services/`, Node.js with only its built-in modules and the TS SDK), and Caddy serving the web wallet (`web/`, the same build) with a
     certificate and the public paths `/rpc`, `/indexer`, `/btc` and `/faucet` on one origin, so the page needs no CORS.
  4. **The seed nodes' local policy**: `--settlement-feerate 2` (the node's default, above signet's `MIN_SETTLEMENT_FEERATE` of 1: see signet's fee market below; the estimate decides above it,
     D-102), `--max-settlement-feerate 5` (2026-10-18; the node's default of 100 before: see signet's fee estimate below) and `--min-block-interval-ms 1000` (devnet's 5 000 would hold a chain whose genesis difficulty matches its hashrate at the floor for good: blocks could never come faster than the target,
     so ASERT would never raise it; one second only bounds the burst after an outage, while ASERT has lowered the difficulty). Their funding outputs are listed from a watch-only wallet of the
     server's bitcoind (`--btc-funding-wallet ksana-node1`, `ksana-node2`) and never split (`--funding-pool-target 0`): the operator funds each address with a few outputs (D-109).
  5. **Genesis config** (`ksana-signet genesis`, written at launch with the timestamp of that moment, ASERT's anchor, D-026; committed as `signet/genesis.json`): anchored `ANCHOR_DEPTH` (1) block below the
     signet tip, like mainnet's (`mainnet/README.md`); 100 000 000 KSA to the faucet and 10 000 000 to an operator account; the block share (D-089) to a signet-only key (no network may start with
     mainnet's placeholder, D-096); `bits` for 5 000 Argon2id evaluations a block with one producing seed node (`0x1f0d1b71`) or 10 000 with two (`0x1f068db8`), an evaluation taking about 1 ms on one
     core (D-094), so a block of weight 1 comes about every 5 seconds from the start, and an empty block of weight w about w times as slowly (D-090).
  6. **KSA faucet** (`signet/services/faucet.mjs`): 100 KSA per account at most once in 24 hours, at most 5 requests a day from one client address and 2 a minute overall after a burst of 120; it signs
     path B transfers from the faucet account at a fee of 1 base unit, the seed nodes' packing floor, and at the node's suggested fee, at most 1 000 base units, only for a transfer the node refuses at
     that fee as below its floor or for a full entry pool and only while the balance still covers it (2026-10-15: at twice the suggested fee before, which raised the suggestion with every round of
     requests, as the suggestion is the median of the lowest path B fee rates of the last 20 blocks and on the signet those are mostly the faucet's own transfers; a refusal for an op already
     waiting at the nonce never raises the fee, as the higher fee would replace that op, after a restart a transfer the faucet already answered). The node's account nonce counts only committed
     ops, so the faucet counts its nonces itself, keeps at most 48 transfers waiting (the entry pool holds 64 per account, M26), and submits a transfer again when it has waited a minute at the
     account's nonce and the node no longer has it, signing it again with a higher fee, up to the same 1 000 base units, when the node refuses it for those two reasons. With two seed nodes it sends
     through node2, so that a client holding the connections of node1, which serves the public `/rpc`, does not cut it off.
  7. **BTC proxy** (`signet/services/btc-proxy.mjs`): four of the bitcoind methods the web wallet's path C and top-ups use (`getblockcount`, `getblockchaininfo`, whose chain the page checks
     before it lists coins or broadcasts (A2-11, D-106), `estimatesmartfee`, `sendrawtransaction` up to 400 000 bytes, 20 a minute per client address), every other method refused. The page lists
     an address's coins through an Esplora API instead (D-108): the proxy passes two GET paths to mempool.space's signet API, `/btc/esplora/block-height/0` (the genesis hash the page checks, kept
     once read) and `/btc/esplora/address/<tb1p…>/utxo` (the address checked as a signet Taproot address first, the answer kept for 5 seconds), and nothing else: concurrent requests for one path
     share one, a client address may make 30 a minute, and all of them together reach mempool.space at most twice a second; mempool.space unreachable or silent for 15 seconds, or an answer of
     another shape, is a 502; an answer goes out with the proxy's own content type (`text/plain` for the hash, `application/json` for coins, `nosniff`), never
     mempool.space's, so that no answer is served as a page in the wallet's origin. `scantxoutset`, the page's fallback when Esplora gives no answer, is refused with a message saying so, which the page shows: on the public signet one scan reads the
     whole UTXO set for 30 to 50 seconds (timed on the server, D-109) and bitcoind runs one at a time, so any client could keep that scan and the server's disk busy with a request every half
     minute, and while mempool.space is down every page user's scan would wait behind the others anyway.
  8. **Web page**: a hosted copy reads `deployment.json` beside `index.html` and starts on that network (node, indexer, BTC proxy) instead of the simulator, unless the visitor chose a data source
     before; Receive gets a button for that network's faucet. A copy without the file behaves as before.
  9. **Deployment over ssh** (`signet/deploy.sh`, runs with the bash 3.2 of macOS): it logs in with the operator's own ssh keys and config as root or a user with passwordless sudo and asks for no key,
     with ssh keepalives (`ServerAliveInterval` 30, `ServerAliveCountMax` 10: a release build prints nothing for minutes, and a proxy or VPN on the operator's side may drop a quiet connection);
     it copies `git archive HEAD` to the server and runs `signet/server.sh` there as root (installs Docker, adds swap for the release build, writes `.env` with the random RPC password, takes
     `<ip-with-dashes>.sslip.io` as the site when there is no domain; a domain given to `setup` must resolve to the server before anything is saved, and an AAAA record is warned about). `launch` refuses
     when node1's funding address holds no confirmed sBTC, writes the genesis config, starts everything and copies the config back for the commit; a launch cut short after writing the genesis config
     goes on where it stopped (the server's config is kept when the local one is missing or the same), and one that would replace another network's config refuses. Every deploy refuses when the
     committed `signet/genesis.json` differs from the server's; `backup` / `restore` carry the keys to a new server (backups are named `ksana-signet-*.tar.gz`, which git ignores); `reset --yes` deletes
     the chain data and the genesis config for a restart from genesis, keeping bitcoind and the keys. The nodes and the indexer mount the config directory read-only, and bitcoind's password reaches them,
     the probe and curl through the environment or standard input, never through a command line (D-109). Added on 2026-10-14, for a change to the page alone and for one to the nodes' storage:
     `web` pushes `HEAD`, builds the web image alone and recreates its container alone (`compose up -d --no-deps web`), then checks that the live page reports `HEAD`'s commit, since a full
     deploy builds every image and a new ksana image restarts the seed nodes and the indexer; `backup-data <label> [--restart]` stops node1, node2 and the indexer with `compose stop` alone (never
     `down`, which would take bitcoind too), saves their data volumes in `backups/<label>-<UTC time>/` with `SHA256SUMS`, copies them to the operator's machine and checks them there, and leaves the
     three stopped for the deploy that follows unless `--restart`; every build first tags the ksana image in use `ksana-prev-<commit>` (the commit from the builds `ksana-builds` records, or the
     image ID for one built before), so that the prune after the build keeps it for a rollback; `--dry-run` reads the server and prints what the deploy, `build`, `web` or `backup-data` would run.
     Added on 2026-10-15, for a change to the faucet alone (an earlier change to its fees went out by hand): `faucet [--wait <minutes>]` pushes `HEAD` and builds the services image through the
     btc-proxy service, which holds its build section (`compose build faucet` builds nothing); then, as the transfers waiting for a block live in the faucet's memory and a restart forgets
     them, it waits until the faucet's `GET /` shows none, at most 10 minutes unless `--wait` (past that it stops and leaves the faucet as it was; a faucet not listening yet, failing as it
     starts or stopped by hand holds none, and a stopped one is started; when Docker or the faucet does not answer a look, or the faucet's answer is not a success, the wait goes
     on), recreates the faucet's container alone (`compose up -d --no-deps --timeout 0 faucet`: its node process, PID 1, ignores SIGTERM) and prints the commit it runs, which
     must be `HEAD`'s; the BTC proxy keeps its container on the image it runs until a full deploy. The build comes before the wait, so that only about a second separates the last look from
     the restart; a request in that second is still forgotten. `ksana-builds` records the services image too, and `--dry-run` covers `faucet`. `--help` after any command but `probe`
     and `logs`, which pass it on to the relay probe and to `compose logs`, prints the usage and runs nothing: `faucet --help` exited 2 in the real server's dry-run, and the other
     commands ignored `--help` and ran (`down --help` would have taken every service down). The audit of this change found that a failed `docker ps` or `docker inspect` during the wait read
     as a faucet that does not run, which would have let the restart go on while transfers waited; the wait now goes on.
  10. **Rehearsal** (`signet/rehearse.sh`): the same Compose files on one machine, bitcoind on a private signet (`-signetchallenge=51`) mined every 15 seconds and a genesis difficulty of 250 evaluations,
      checked through the page's origin: keys, genesis, the page and `deployment.json`, the BTC proxy, a faucet transfer arriving, its block settled on BTC, the indexer agreeing with the node at every
      height, and the relay probe.
  11. **`docs/SIGNET.md`** becomes the guide for joining: the web wallet with Unisat or Xverse, a node and an indexer of one's own, CPU mining with or without a BTC funding key, the CLI, and what to
      record from the real-wallet tests. `signet/README.md` is the operator's guide.
- **Rationale**:
  - The public signet's miners, mempool policy and other users are what mainnet will meet; a self-hosted signet would only show that our own bitcoind relays our own transactions.
  - One server, deployed from the project owner's machine with the project owner's own ssh login, keeps every key (the seed nodes', the faucet's, the block share's) on that server and in the backups
    the project owner takes; nothing is handed to anyone, and the deploy script holds no secret.
  - Building the images on the server from the committed tree avoids a registry and makes the deployed code exactly a commit; a release build on 2 cores takes about half an hour, done once per deploy
    that changes Rust code.
  - The faucet and the BTC proxy are small, stateless and use the SDK the wallet uses, so they sign and encode with the same code as the page; the proxy keeps bitcoind's RPC off the internet.
- **Costs and limits**:
  - One server is one point of failure for the seed node, the public RPC, the faucet and the page; anyone can still run a node, an indexer and a miner (`docs/SIGNET.md`), and the backup restores the
    network's keys onto another server.
  - The faucet can be drained by many accounts and client addresses; 100 million KSA are a million requests, and a restart from genesis refills it.
  - The seed nodes do not scan signet's UTXO set for their funding: the first version did, with the rescue thread scanning on every BTC poll round, not once per signet block as written here at first;
    the wallets of D-109 replaced it before the launch.
  - An idle network still sends a settlement pair about every two signet blocks (the unlock cycle, D-109): about 0.0007 sBTC a day for the network at the seed nodes' floor of 2 sat/vB, shared by
    the producing seed nodes; `status` shows what is left and which seed node to top up (D-109), and a seed node with no funds keeps producing while the other carries its bodies (D-077).
  - Signet's fee market: the seed nodes' floor and the probe's rate were 1 sat/vB at first, signet's minimum, on the view that signet's blocks are rarely full. On 2026-10-13 the probe's first run on
    the server split its funds at 1 sat/vB at 15:42 UTC, and the split (326 vB) still waited 45 minutes later: signet had mined blocks 326 208 to 326 212 meanwhile, the last three taking nothing
    below 2 sat/vB at about 2.82 million weight units each, while mempool.space showed about 7 500 transactions (9.1 million vB) waiting, about 4 800 of them at exactly 1 sat/vB. So the floor went
    back to the node's default of 2, and the probe pays the automatic rate above, at least 3. At 3 sat/vB the probe's five tests need about 0.0029 sBTC, two thirds of it for the 65 536-byte
    `OP_RETURN`, more than the 0.002 the probe address had for the 1 sat/vB rate. The run on the server took all five from a new output of 0.005 sBTC at the automatic rate, which
    bitcoind's estimate made 5 sat/vB (about 0.0047 sBTC), and signet's miners mined them in the next block; the waiting split was left in the mempool (`--replace` would have spent its
    input again instead). The operator's guide gives the probe 0.005 sBTC since.
  - Signet's fee estimate (2026-10-18): the seed nodes paid the floor until 2026-10-15 about 12:00 UTC; then bitcoind's estimate rose to 13 sat/vB, reached the cap that evening (19:12 UTC),
    fell back to 11 to 16 overnight, rose to 51 the next afternoon, and from 2026-10-16 about 15:45 UTC every settlement pair paid the cap, then the node's default of 100 sat/vB: about 37 000
    sats a pair against about 1 000 at the floor, 0.022 sBTC a day for the network, which spent a top-up of the seed nodes' funding within two days. Signet's blocks never asked for it: from 2026-10-15 to 2026-10-18 every block took transactions below 5 sat/vB and most took some at 1, while filling
    13 to 63 % of their weight on average over three hours, and mempool.space recommended 1 sat/vB. The server's bitcoind answered `estimatesmartfee` with 100.03 sat/vB for every target from 1 to
    24 blocks and 0.1 for 144: the average rate of the seed nodes' own settlements (100.0 to 100.4 sat/vB), which confirm in the next block. That the estimate stood on the seed nodes' own
    transactions was inferred, and `estimaterawfee` on the server that evening bore it out (D-117): a signet's thin traffic makes them a large share of what its estimator sees, which mainnet's is not. The
    seed nodes' cap went down to 5 sat/vB, two and a half times the floor and above every block's lowest rate over those days: a pair costs at most about 2 000 sats, about 0.0015 sBTC a day for
    the network, whatever the estimate says. Congestion on signet above 5 sat/vB would hold settlements back until it clears (the fast lane pauses at the settlement backlog limit, D-046), and the
    operator would raise the cap. The page's path C and top-up transactions take the node's rate (`ksana_estimateFee`), so they were offered 100 sat/vB too and follow the cap down; the relay
    probe's automatic rate has no cap (`signet/README.md`).
  - `/rpc` and `/indexer` run on jsonrpsee's defaults (100 connections, 1 024 subscriptions per connection, unlimited batches) and Caddy adds no limit: one client can hold node1's connections and cut
    the page off, and every subscription is pushed with every block at the server's expense. Node flags for these limits are a follow-up; the faucet avoids node1 when there are two seed nodes.
  - The compose network has no IPv6: with an AAAA record, every IPv6 visitor would reach the services from one gateway address and share the faucet's and the BTC proxy's per-address limits.
  - `-maxuploadtarget` caps only bitcoind's upload, not Ksana's P2P or the HTTPS traffic; on a server with a monthly traffic package the provider's traffic alarm is the limit (`status` shows the
    interface's totals since boot).
  - The page's coin listing depends on mempool.space: while it does not answer, path C and top-ups on the hosted page cannot list coins, since the proxy offers no scan (item 7); a page pointed at
    a signet bitcoind of one's own still scans (D-108). The first version passed `scantxoutset`, one scan at a time with a pause as long as the last one took; the scan timed on the server (D-109)
    removed it before the launch, at the project owner's coordinator's request.
  - The genesis difficulty assumes 1 ms per evaluation; on a slower core the first blocks come slower, and on a faster one sooner, until ASERT corrects it (half-life 2 hours). At the launch the
    first five blocks came about 4 to 5 times as fast as it expects (PROGRESS M32).
  - Signet's miners may change their policy at any time; the probe can be run again at any time (`deploy.sh <host> probe`).
- **Impact**: no consensus change; the crates gain the probe example here, and the funding changes of D-109. New: `signet/` (`Dockerfile`, `docker-compose.yml`, `rehearsal.yml`, `Caddyfile`, `deployment.json`, `.env.example`,
  `ksana-signet.sh`, `server.sh`, `deploy.sh`, `rehearse.sh`, `services/` with its tests, `README.md`), `crates/ksana-btc/examples/relay_probe.rs`, the web page's `app/deployment.ts` and the faucet
  button in every language. The images (`bitcoin/bitcoin` 31.1 and `caddy` 2.11.7 pinned by digest, `rust` 1.98.1, `node` 22, `debian` bookworm) are deployment, not dependencies of the code; the
  services add no npm package. CI's ts sdk job runs the services' tests. SPEC §12 (repository layout), §14 (M32); PROGRESS M32; CLAUDE.md; D-108's revision note, `docs/WEB_DESIGN.md` §7
  and `web/README.md` on the proxy's refusal of scans. Once the network runs, `signet/genesis.json` is committed, and every later restart from genesis commits a new one.

## D-108 The signet page lists the wallet's coins through Esplora when its Bitcoin RPC offers it, and scans otherwise (2026-10-13, for the public signet of D-107; web page only, no consensus change, M31)

- **Problem**: on the public signet the page's Bitcoin RPC is bitcoind behind the hosted proxy (D-107), and `scantxoutset` walks the whole UTXO set: tens of seconds to a minute per call. bitcoind runs
  one scan at a time and the proxy queues them, so with a few users at once, listing the coins for path C or a top-up takes minutes.
- **Decision**: on the signet page `BtcRpc.utxosOf` first asks an Esplora interface under the Bitcoin RPC's URL (`<url>/esplora`, which the public signet's proxy is to forward to mempool.space's
  signet API; until it does, the hosted page scans). `block-height/0` must be signet's genesis block (`00000008819873e925422c1ff0f99f7cc9bbb232af63a077a480a3633bee1ef6`); then
  `address/<address>/utxo` lists the coins, of which only confirmed ones are spent, as with a scan. An Esplora whose genesis block is another chain's is refused with `BtcChainMismatchError`, like a
  bitcoind on another network (D-106). No Esplora, an error status, or an answer that is not a block hash or a well-formed UTXO list falls back to `scantxoutset` after the `getblockchaininfo` check.
  Broadcasting, the fee rate and the check before signing stay with bitcoind (`sendrawtransaction`, `estimatesmartfee`, `getblockchaininfo`). The devnet page is unchanged.
- **Rationale**:
  - Esplora answers from its address index at once.
  - Its answer is only a list of candidates. The wallet's Taproot signature commits to every input's amount and script (BIP341), so a wrong list cannot get a valid spend of other coins or amounts
    signed, and the transaction goes to the checked bitcoind, which refuses inputs it does not have.
  - The genesis block tells signet from mainnet, testnet3, testnet4 and regtest in one request the proxy can cache for good (it does not tell one signet from another, see the limitations).
  - Falling back keeps the page working behind a proxy without Esplora, and with a signet bitcoind of one's own.
- **Costs**: the hosted page's coin list depends on mempool.space's signet API through the proxy, with the scan as the fallback; a listing takes two requests instead of one; the proxy must forward
  `block-height/0` and `address/<address>/utxo` under `/btc/esplora`, and only those (the address checked as a `tb1p…` string), caching the genesis block for good and a list for a few seconds at
  most, with a limit per client and a timeout: every page user reaches mempool.space from the proxy's one address, so its limits apply to them together (a 429 falls back to a scan).
- **Limitations** (from the audit review of this change; neither is closed, as only test coins are at stake on signet):
  - Spending only confirmed outputs rests on Esplora's word. bitcoind accepts inputs from its mempool too, so an Esplora that marks an unconfirmed output as confirmed gets it spent, and the
    transaction confirms only with its parent, late or never; a top-up's payment confirming after `payBy` still pays the seller while the claim fails. That takes a fault at mempool.space or at
    the proxy's operator. Checking each picked input before signing with bitcoind's `gettxout <txid> <vout> false` (a lookup in the confirmed UTXO set, not a scan, which also returns the amount
    and the script) would leave Esplora's list a mere hint; the proxy would have to let `gettxout` through.
  - The genesis block does not tell one signet from another: every signet shares it (BIP 325), as every signet's bitcoind reports `chain` `signet`. So neither this check nor D-106's
    `getblockchaininfo` check refuses a custom signet (Mutinynet, say). For Esplora that costs at most a refused broadcast, since bitcoind decides; a Bitcoin RPC on a custom signet would put a
    path C transaction or a top-up's payment on that chain. Pinning both to a block the node knows would name the exact chain (bitcoind's `getblockheader` of the node tip's `btc_anchor` with
    at least one confirmation, Esplora's `block/<hash>/status` with `in_best_chain`), a hardening to consider before the page supports mainnet.
- **Revised by D-107** (M32): the public signet's proxy refuses `scantxoutset` (a scan takes 30 to 50 seconds there, D-109), so on the hosted page an Esplora request that fails, or that the
  proxy's limits refuse, ends in that refusal, whose message the page shows instead of the coins. The page is unchanged and still scans through any other Bitcoin RPC.
- **Impact**: `web/src/wallet/btc.ts` (`BtcRpc.utxosOf`, `esploraUtxos`, `parseEsploraUtxos`, the genesis blocks by chain); `web/src/wallet/btc.test.ts` (signet's Esplora used without a scan, another
  chain's refused, no Esplora or a malformed answer scanned instead, never asked on the devnet page, path C listing from Esplora and broadcasting through bitcoind); `docs/WEB_DESIGN.md` §7,
  `web/README.md`; PROGRESS M31.

## D-109 The producer's funding outputs: one scan per BTC tip, a failed scan costs settlements and not blocks, and a watch-only wallet of bitcoind instead of scanning (found by the audit of the hosted signet on 2026-10-13, the wallet chosen by the project owner's coordinator the same day; revises D-037, D-075, D-107 and SPEC §7.7; local policy, no consensus change, M32)

- **Problem**: the audit of the hosted signet (D-107) found that a producing node reads bitcoind's whole UTXO set far more often than D-107 said. The producer scans its funding address with
  `scantxoutset` once per anchor, but the rescue thread ends every BTC poll round (500 ms by default) in the funding pool's upkeep, which scanned again whenever no split was in flight: with the default
  `--funding-pool-target` 64, about twice a second in steady state. Measured: with the audit's counting client around the mock chain, 99 scans in 5 seconds of a 50 ms poll with a full pool of 4, none with
  the target 0; in the signet rehearsal (D-107), bitcoind's log shows 82 `scantxoutset` calls in one minute. bitcoind runs one scan at a time; signet's chainstate is about 5 GB (mainnet's about 14 GB), so
  a scan takes tens of seconds there: timed on the hosted server on 2026-10-13, at signet height 326 207 with 79 839 266 outputs in the set, one `raw()` script took 48.9 seconds cold and 30.6
  seconds warm. A producer whose scan met another one retried "Scan already in progress" for 5 seconds and then failed the
  whole attempt to produce, so the two seed nodes and the web wallet's scans through the BTC proxy could keep each other from producing at all, and a scan longer than the RPC client's fixed 60-second timeout
  would stop a funded producer for good. On mainnet the same code would stop every funded producer from the start.
- **Decision**:
  1. **One scan per BTC tip** (`ksana_btc::client::RpcClient`): a scan is kept with the best block `scantxoutset` reports having read and answered again, for every caller, until bitcoind's best block
     changes; a caller arriving while a scan runs waits for it (bitcoind would refuse a second one) and takes its result when it asked for the same script; at most 16 scripts are kept. The
     producer and the rescuer of a node thus share one scan per tip.
  2. **A failed scan costs settlements, not blocks** (`producer.rs`): the block is produced without funding, so it carries no settlement transactions and its body waits for a later settlement (D-077),
     and the producer asks again at the next anchor or after 30 seconds (`FUNDING_RETRY`).
  3. **A watch-only wallet** (`--btc-funding-wallet <name>`, `RpcClient::with_wallet`): the funding outputs are listed by a wallet of the same bitcoind that holds the descriptor `raw(<funding locking
     script>)`, which bitcoind keeps current block by block, instead of a scan of the UTXO set. `listunspent` (one confirmation, immature coinbases included) names the outputs; `gettxout` checks each is
     still in the UTXO set and says whether it is a coinbase; the height follows from the confirmations. Outputs the wallet listed once are asked again with `gettxout` until they leave the UTXO set: a
     descriptor wallet hides an output spent by a transaction that left the mempool unconfirmed (a dropped settlement of a block that left the main chain) until that transaction is abandoned, although
     the output is spendable. At startup the node asks the wallet with `getaddressinfo` whether it watches the funding address and refuses to start when it does not. Without the flag nothing changes.
  4. **The hosted signet** (D-107): `ksana-signet.sh` creates one wallet per seed node (`ksana-node1`, `ksana-node2`) and one for the relay probe (`ksana-probe`), blank and without private keys,
     loaded at every start of bitcoind, and imports the descriptor with the key file's time as its birth (bitcoind rescans from there; a wallet that already watches the address is left alone), at
     `keys`, before a node or the probe starts, and at `status`; the seed nodes run with `--btc-funding-wallet` and `--funding-pool-target 0`, so no split ever spends their outputs, and the probe
     takes `--wallet` and pays its automatic rate, at least 3 sat/vB (D-107), so a run of its five tests needs 288 488 sats and spends about 277 000 (1 sat/vB at first: 103 816 and about
     93 000); a split of its funds that waits in the mempool is replaced with `--replace <txid>`, since the wallet no longer lists the coins it spends. bitcoind's password reaches the node, the
     indexer and the probe through `KSANA_BTC_RPC_PASS` (`--btc-rpc-pass` stays), and the script's curl through standard input.
  5. **Funding the seed nodes**, from the node's rules: empty blocks carry no settlement, but an idle network still sends one settlement pair about every two signet blocks. An unlock entry needs a body
     (`ksana_btc::payload::needs_body`), the producer writes one whenever the settled height at its anchor has passed the unlock cursor (`Chain::unlock_entries`), so the block carries a pair, and
     that block's own reward can be unlocked once the BTC height where its pair landed is at or below the anchor (with `ANCHOR_DEPTH` 1, from the next signet block on), by a block that carries the
     next pair. The producer of the block carrying a pair pays for it, so two producing seed nodes share the cost by hashrate.
     - How often: 62–72 pairs a day for the network while idle, 144 at most while ops keep two such chains of pairs going.
     - What a pair costs: 316–383 vB with an idle chain's headers and an unlock entry, plus the reveal's 240-sat P2A output: 872–1 006 sats, about 1 000, at 2 sat/vB, the seed nodes' floor
       (D-107), and more while bitcoind's estimate is above it (D-102). The least that funds a pair is about 1 300 sats at 2 sat/vB (the fees, the P2A output and the change's dust floor of
       330 sats). The seed nodes' floor was 1 sat/vB at first, about 600 sats a pair, until signet's blocks took nothing below 2 (D-107).
     - Four weeks: about 0.017–0.020 sBTC for an idle network at 2 sat/vB, up to twice that with traffic.
     - Per producing seed node: at least three outputs (one for the pair in flight, one for a second pair while ops keep two going, one for a CPFP child, a rescue or a republish; every pair
       returns its change to the address, so the outputs are reused); at least 0.006 sBTC to start, 17–20 days of half an idle network's pairs at 2 sat/vB (about 9 days for a seed node
       alone); 0.02 recommended, four weeks at 2 sat/vB with traffic.
     - What the operator sees: `ksana-signet status` lists each seed node's funding from its wallet (confirmed, pending change, outputs) and sets `top_up` below 0.002 sBTC or three outputs
       (about six days of half an idle network's pairs at 2 sat/vB); when no seed node holds the 1 400 confirmed sats a pair may need, it warns that nothing lands on signet, so BTC
       finality and the indexer stand still until a seed node is funded (one seed node without funds keeps producing while the other carries its bodies, D-077).
  6. **The wallet is required on signet and mainnet** (`check_funding_listing` in the node's `main.rs`): a node given `--btc-funding-key-file` on signet or mainnet refuses to start without
     `--btc-funding-wallet`. A scan of signet's UTXO set took 30 to 50 seconds on the server (above), mainnet's set is larger, and the producer scans in its block production path, so without a
     wallet every new BTC tip would hold the node's blocks back for the length of a scan, or for the RPC client's 60-second timeout. devnet, on regtest's small UTXO set, still scans.
- **Rationale**:
  - bitcoind's wallet is an index of one address's outputs that bitcoind already keeps current; following the funding outputs in the BTC blocks the tracker reads would rebuild a wallet in the node.
  - The cache needs no new rule: the UTXO set changes only with the tip, and `scantxoutset` reports the tip it read.
  - A producer without funding for a while is a case the rules already handle (D-077); stopping block production over a busy bitcoind was never intended.
  - The pool's automatic splits (D-075) were the other scanner and serve a busy producer; the seed nodes' addresses are funded with several outputs by hand instead.
- **Costs and limits**:
  - A node restarted while an output is hidden by a dropped transaction does not see it again until the transaction is abandoned in the wallet (`bitcoin-cli -rpcwallet=ksana-node1 abandontransaction
    <txid>`) or the output's coin moves; the coins are not lost.
  - Only devnet still scans: on signet the cold scan came within about 11 seconds of the RPC client's fixed 60-second timeout (corepc-client's) while the UTXO set keeps growing, so signet and
    mainnet require the wallet (decision 6; `docs/SIGNET.md`, `mainnet/README.md`). Where a scan does time out, it costs the node its settlements until the next attempt (decision 2), not its blocks.
  - A wallet keeps the transactions that spend its outputs and, with bitcoind's default `-walletbroadcast=1`, puts them back into its own mempool when it loads or imports (without sending
    them) and sends them to its peers every 12 to 36 hours while they are unconfirmed (Bitcoin Core 31.1's periodic resend), including settlement pairs the node itself dropped (a pair of a
    block that left the main chain): such a pair can still confirm, spending outputs the node may since have used again, so one of the two transactions fails. Found by the second audit round of the hosted signet; the hosted signet is to add `-walletbroadcast=0` the next time bitcoind's service definition changes
    for another reason (changing it now would restart bitcoind), or a node could abandon the pairs it drops in the wallet (`abandontransaction`).
  - The wallet must exist before the funding confirms, or be given a birth time before it (the import rescans from there; the key file's time is always early enough).
  - The idle unlock cycle is the node's behaviour on mainnet too, where `ANCHOR_DEPTH` 2 makes it one pair every three or four BTC blocks: about 15 000–17 500 pairs a year while idle, at
    mainnet's fee rates. Follow-up, not part of this decision: a local policy that writes fewer unlock entries while the network is idle, or merges unlocks.
- **Impact**: no consensus change. `ksana-btc`: `RpcClient` (the per-tip cache, `with_wallet`, `wallet_watches`), the trait documentation of `scan_utxos`, client tests; the relay probe (`--wallet`, the
  password from the environment, a reorg recheck of the transactions it found missing, the fee advice when some are not mined). `ksana-node`: `--btc-funding-wallet` (required on signet and mainnet,
  `check_funding_listing`), `FUNDING_RETRY`, the password from the environment, `m10_funding_pool.rs`; `ksana-indexer`: the password from the environment. `signet/` (D-107); SPEC §7.7; `docs/SIGNET.md`, `signet/README.md`, `mainnet/README.md`; PROGRESS M32; CLAUDE.md.

## D-110 The web page's history lists read older blocks on request, as the node has no listing of an account's operations (2026-10-14, asked by the project owner when the public signet's account page listed only the operations from block #125 on; web page only, no consensus change, M34; since D-111 only from a node without the listings)

- **Problem**: on the node data source the page caches the latest 60 main-chain blocks and backfills down to the BTC-final boundary (D-069, `docs/WEB_DESIGN.md` §7.1). The node offers no
  listing of an account's operations (D-069's `ksana_getAccountOps` is still a suggestion), so the account page's history, the operations page's latest list and the wallet's activity listed only
  the operations in the cached blocks. On the public signet on 2026-10-14, with its tip at #184 and the BTC-final boundary at #162, the project owner's account page listed one of the account's seven
  operations, under "Only operations in blocks from #125 onward are listed": the KSA it received in blocks #54 to #74 and the 10,000 Wizards of block #81 were in its balances and in no list. Nothing
  was lost; the node serves those blocks as before.
- **Decision**: a page that lists history and whose list is not full (50 operations on the account page, 60 on the operations page, 40 in the wallet's activity) asks the data source for older
  blocks (`RpcChain.wantHistory`). The sync loop then reads the main chain by height below the cache, down to 2 000 blocks below the tip (`HISTORY_DEPTH`), in rounds of 120 blocks with 12
  requests in flight, one round after another, and the list grows after each. Above the list the page says how far down it has read while it reads ("Fetching older blocks (at #h)…") and, once it
  stops above the genesis block, which blocks the list covers, with a button that reads another 2 000 (`loadEarlier`). The request lapses ten minutes after a page last asked; until then the cache's
  pruning keeps the blocks it covers. A full list asks for nothing, and the other pages read what they read before.
- **Rationale**: the node already serves every main-chain block by height, and a young network is short and mostly empty blocks: against the signet node (tip #190), the page built on this
  change listed the account's seven operations about five seconds after the first sync. Reading only for the lists keeps the other pages' cost, and the node's, where it was.
- **Costs and limitations**: a visitor who opens one of these lists reads up to 2 000 blocks (as many `ksana_getBlock` calls, and `ksana_getSettlement` for the blocks with entries), and an older
  history takes a click per 2 000 blocks. A long-lived chain needs the listing D-069 suggests, from the node or the indexer; this does not add it.
- **Impact**: `web/src/chain/rpc/chain.ts` (`wantHistory`, `loadEarlier`, `historyLoading`, the backfill's floor, pruning, the page shown after each round; the closures are computed lowest block
  first, so that their recursion stays shallow however long the cached chain is); `web/src/components/HistoryReach.tsx`; the account, operations and wallet pages; the messages in six languages
  (`ui.history`, replacing `account.loadedOnly`); `web/src/chain/rpc/chain.test.ts` (a synthetic node: nothing changes without a history list, the whole chain read in rounds with every block
  fetched once, the depth and `loadEarlier`, the lapse); `docs/WEB_DESIGN.md` §7; PROGRESS M34.

## D-111 The node lists the operations of an account and of the whole chain, and the web page's history lists page through them (2026-10-14, asked by the project owner after D-110; revises D-069, D-110 and SPEC §10.2, §11.2, §11.3, `docs/RPC.md`, `docs/WEB_DESIGN.md` §7; node storage, JSON-RPC, TS SDK and web page, no consensus change, M35)

- **Problem**: D-110 has the web page read the main chain block by block below its cache while a history list is not full, 2 000 blocks below the tip and 2 000 more per click. The cost grows
  with the chain: an account whose operations lie 100 000 blocks below the tip takes 50 clicks and 100 000 `ksana_getBlock` calls to list. D-110 left the real fix, the listing D-069 suggested
  from the node or the indexer, open; the project owner asked for it the same day.
- **Decision**:
  1. **The node provides it.** It keeps every block and receipt of its main chain on disk (pruning bounds memory only, D-097) and already writes its op index (`ops`: op → where it executed) in
     the batches that change the main chain, so a listing can follow the main chain the same way. The indexer runs on BTC data only and executes a block once it is settled and
     `SETTLEMENT_DEPTH` blocks deep or BTC-final (D-082), so its lists would lag the page's newest blocks; a deployment need not run one, and the page would need a second endpoint for its lists.
  2. **Positions.** An op on the main chain has the position (height, slot, entry): the height of the main-chain block that accepted the block including it, that block's slot in the
     acceptance order of the height (the blocks the main-chain block merges, in consensus order, then the main-chain block itself, the order of `MainEntry::accepted`) and the entry's index.
     Positions sort in the order the main chain executes entries; their JSON form is the string `height:slot:entry` in decimal, without leading zeros (`ksana_rpc::OpPosition`).
  3. **Storage** (the block store's local format, `crates/ksana-node/src/store.rs`): the column family `op_seq` maps each position holding a path B or path C entry (unlock entries are not
     listed) to the op ID, the block, the outcome (success, failed, skipped) and the accounts it concerns, and `account_ops` holds the key account ‖ position for each of those accounts: the signer,
     and when another account, the receiver of an asset transfer or the contract called (a deployment, under its signer only). Positions are written big-endian, so keys sort in position order.
     `connect_main` writes a height's entries, and `rewind_main` deletes those of the heights it rewinds, in the same synchronous batch as the main chain's tip; `meta` `op_index` holds the tip and
     height the index describes, and the index is written only while it describes the stored tip; while it does not, both methods answer `-32601`, as a node without them does (decision 6).
     When the node opens its store and the index does not describe the tip (a data directory from an earlier version, a crash during a build, or a main-chain block whose accepted blocks were
     not in the summary's order, which logs an alert and leaves the index behind), it clears the index and builds it again from the stored summaries, blocks and receipts before anything else
     starts, and logs it; a build that fails is logged, and is tried again at the next start.
  4. **The methods**: `ksana_getAccountOps(account, before?, limit?)` and `ksana_getLatestOps(before?, limit?)` list the ops below `before` (the newest when omitted), newest first, at most `limit`
     (1 to 500, 50 by default), with `next` (where the next page starts, `null` at the genesis block) and `at` (the tip the page was read at). A page is read from one RocksDB snapshot. An op
     included by several blocks has an index entry at each, and the page shows it once, at the entry where the op index places it (`ksana_getOpStatus`'s position: where it executed, or skipped
     where no block executed it), passing over the others; a page passes over at most 1 024 such entries and then ends with `next` set, so that one call's work stays bounded. The status is
     `failed` or `skipped` from the receipt, otherwise `fast_confirmed` or `btc_final` by the finality of the position's height, as in `ksana_getOpStatus`. Both ask it in constant time
     (`NodeRpc::btc_final`: a height below the main chain's window is final, and above it the finality decision's block at the height is an index into a vector), without walking the window
     as the settled closure, which `ksana_getSettlement` reports, does.
  5. **TS SDK** (`sdk/ts/src/rpc.ts`): `KsanaRpcClient.getAccountOps` / `getLatestOps`, `OpPosition` with `parseOpPosition` / `formatOpPosition`, results parsed strictly like the others, and
     `RPC_ERROR_CODES.METHOD_NOT_FOUND` (`-32601`, from a node before this decision or one whose index is not built).
  6. **Web page** (`web/src/chain/rpc/chain.ts`): a history list that is not full (the account page's 50, the operations page's 60, the wallet's 40) reads the next page of its listing
     (60 ops) and fetches the blocks of the listed ops the cache lacks, by hash, 12 at a time; it reads up to 600 ops of its listing, because a filter (the account page's sent and received tabs)
     can match few of them, and then says how many it has looked through, with a button that reads 600 more. A page that failed is shown with a button to try again, and asked again 8 seconds
     later while the list is open. The blocks a listing fetched stay in the cache while a page asks, and a listing lapses ten minutes after a page last asked. A node that answers a listing with
     `-32601` (a node before this decision, or one whose index is not built) has the page read blocks as D-110 does, for every history list, as long as the page stays connected to it; any
     other error (an internal error such as a failed storage read, no answer, an HTTP error) is a failed page.
- **Rationale**:
  - A list now costs one call per 60 ops and one `ksana_getBlock` per block holding them, however deep they lie, and the node answers from an index instead of reading blocks. On a local
    two-node devnet at block #450, an account page whose 50th operation lay 273 blocks below the tip fetched 113 blocks in all, its cache of the latest 60 included, against 322 when it read
    blocks as D-110 does.
  - A position, rather than a page number or an offset, stays valid while blocks arrive on top, and puts the height first, so a reorg changes only the positions at and above its fork.
  - Writing the index in the batches that move the main chain, as the op index is, means a listing never shows an op of a block that is not on the stored main chain, and needs no catch-up
    process of its own; the marker makes an index that missed a batch visible instead of wrong.
  - Showing an op once where `ksana_getOpStatus` places it keeps the two answers in agreement, which the page relies on: it fetches the block a listed op names and finds the op there.
  - A page asks the status of up to 500 heights. The settled closure walks every accepted block from the window's base up to the height, so asking for it per listed op, as the pull request
    first did, cost a page up to 500 walks of the window: by the measurement in its review, about 2.5 s of CPU for a page of 500 heights over a window of 21 000 blocks, about what a pruning
    node keeps at the 5-second block target. The status needs only the finality decision, which is an index.
  - A node whose index is not built has no listings until a later start builds it, as an older node has none, so it answers with the same code and a client needs one fallback. Any other
    error can pass, and fails one page instead of having the page read blocks until it is reloaded.
  - The accounts are those the page already associated with an op (signer, transfer receiver, contract called), so the lists show what they showed for cached blocks.
  - Nothing in it is hashed, signed, sent to peers or executed: nodes may differ in having it, and it changes no rule.
- **Costs and limitations**:
  - Disk: an op concerning two accounts takes a 16-byte key and a 133-byte value in `op_seq` and two 48-byte keys in `account_ops`, 245 bytes before RocksDB's own overhead per entry (the
    block store is not compressed, D-103); an op concerning one account, 165.
  - An older node cannot open the data directory again: RocksDB refuses to open a database without naming all its column families, as with the column families of D-097 and D-101. Going
    back to a node binary from before this decision needs one of: a copy of the data directory taken before the upgrade, with the node stopped (and the previous image kept); a new data
    directory and a sync; or both column families dropped together with `meta` `op_index`. Dropping the column families alone is not enough: if the older binary then leaves the tip where the
    marker says, the next upgrade takes the empty index for a built one.
  - The first start of this version on an existing data directory reads every main-chain block and its receipts once before the node syncs or serves; on a long chain that start takes longer.
  - Accounts reached only inside a call are not listed under them: the receiver of a token contract's transfer, a top-up's beneficiary, a contract created by a factory. A token transfer is
    listed under its sender and the token contract. Telling them apart needs the contracts' events, which have no standard form; the page's lists never showed them either.
  - Pages read at different tips join without gaps or repeats only while the main chain at and below the height of `next` stays the same; a client that sees a reorg that deep lists again from
    the newest. The page's lists keep the cached blocks near the tip in any case.
  - Inherited from the op index: an op skipped in a merged block (for instance a path B op paused by D-046) and executed higher up has its location at the execution; when a reorg disconnects
    the block that executed it, `rewind_main` deletes the location although the skipped entry remains on the main chain, so the listings pass over that entry, as `ksana_getOpStatus` calls the
    op unknown, until a block includes the op again. Restoring the location would mean searching the signer's entries for the op's other positions in every rewind; listing the entries that
    have no location could list an op twice (one skipped by two merged blocks).
- **Impact**: `crates/ksana-rpc` (`OpPosition`, `OpRefJson`, `OpListJson`, `DEFAULT_OPS_LIMIT` / `MAX_OPS_LIMIT`, `ops_limit`, `Backend::account_ops` / `latest_ops`, the methods, unit tests);
  `crates/ksana-node` (`store.rs`: the column families `op_seq` and `account_ops`, `meta` `op_index`, `list_ops`, `rebuild_op_index`, `connect_main` / `rewind_main` / `put_genesis`, unit tests;
  `chain.rs`: the build when the store is opened; `rpc.rs`: the methods and `btc_final`, which `ksana_getOpStatus` uses too; `tests/m35_op_index.rs`: merged blocks in acceptance order with an
  op included twice listed once, a reorg, a data directory without the index, and the JSON-RPC paging, cursor, errors and BTC-final statuses over a node process); `codes::METHOD_NOT_FOUND`
  in `ksana-rpc`; the RPC mocks of `ksana-rpc` and `ksana-cli` tests; `sdk/ts` (`rpc.ts` and its tests); `web/`
  (`chain.ts`: `wantHistory(key, more)`, `loadEarlier(key)`, `history(key)`, the D-110 reading kept for nodes without the listings; `HistoryReach.tsx`; the account, operations and wallet pages;
  the messages in six languages; `chain.test.ts`); SPEC §10.2, §11.2, §11.3; `docs/RPC.md`; `docs/WEB_DESIGN.md` §7; PROGRESS M35; CLAUDE.md.

## D-112 The public signet moves to signet.ksana.network, its former name keeps the endpoints, and the project's site is on Cloudflare (2026-10-14, asked by the project owner after registering `ksana.network`; revises D-107's domain and `docs/SIGNET.md`; deployment and documentation, no consensus change, M32)

- **Problem**: the hosted signet (D-107) answered at a name outside the project, and `docs/SIGNET.md` wrote it as `<signet host>`. On 2026-10-14 the project owner
  registered `ksana.network` (registrar and DNS on Cloudflare) and asked for it to be used, with a landing page and docs. The old name was already in use: the CLI, the SDK and pages were set
  to its `/rpc`, `/indexer`, `/btc` and `/faucet`.
- **Decision**:
  1. **Names.** `signet.ksana.network` is the signet: the web wallet and explorer, `/rpc` (HTTP and WebSocket), `/indexer`, `/btc`, `/faucet` and the seed nodes' P2P ports
     (`/dns4/signet.ksana.network/tcp/9000`). `ksana.network`, with `www.ksana.network`, is the project's site and docs (M37). The name it answered at before is the signet's former name.
  2. **DNS and certificates.** `signet.ksana.network` has one `A` record pointing at the server, DNS only (not proxied), and no `AAAA` record; Caddy takes its certificate from Let's
     Encrypt as before. It is the only name of the zone that points at the server.
  3. **The former name** (`KSANA_SIGNET_FORMER_DOMAIN` in `.env`, `deploy.sh setup --former <name>`): Caddy serves it with a certificate of its own; `/rpc`, `/indexer`, `/btc` and `/faucet`
     answer there as at the domain, and every other path (the page, `deployment.json`, the assets) moves to the domain with a 301, path and query kept. `setup` checks the former name as it
     checks the domain (refused when given and not resolving to the server alone, warned about when kept from `.env`), refuses the domain as its own former name, and `--former none` drops
     it. The Caddyfile's host matcher reads the name per request (`{env.KSANA_SIGNET_FORMER_DOMAIN}`): read with the file, an empty name would leave a matcher without a host, which Caddy
     refuses at start, and the rehearsal has none.
  4. **The site and docs are on Cloudflare** (the project owner, the same day: first Cloudflare Pages, then "whatever can go into Cloudflare Workers, put it into Workers"): Cloudflare
     Workers serve them from the repository, their names point at the Worker, and `www` moves to `ksana.network` on Cloudflare's side; none of it runs on the signet's server.
  5. **The signet's page stays on the server**, on the same origin as its endpoints (the project owner's choice the same day, after the comparison below).
- **Rationale**:
  - The endpoints stay at the former name because their clients cannot follow a move: the CLI's JSON-RPC client follows no redirect, `fetch` (the SDK, pages) repeats a `POST` moved by a 301
    as a `GET`, WebSocket clients follow none, and a browser follows no redirect from a CORS preflight. A visitor's page and bookmarks move, as browsers follow a 301 for a `GET`.
  - DNS only rather than Cloudflare's proxy: the proxy carries HTTP alone, so the seed nodes' P2P ports (9000, 9001) and bitcoind's (38333) need the name to reach the server anyway; behind
    the proxy the faucet and the BTC proxy would see Cloudflare's addresses instead of their clients' and lose their per-address limits; and Caddy's certificate and `setup`'s check need the
    name to resolve to the server.
  - A Worker in front of the signet's page (its files from Cloudflare, the endpoints proxied to the server) was weighed and not taken. The page polls every 2.5 s with at least four requests
    (the latest block, the fee, the indexer's status, the BTC height; `web/src/chain/rpc/chain.ts`), so one page left open makes about 138 000 requests a day through the Worker, above Workers
    Free's 100 000 a day, past which every call fails with error 1027 until midnight UTC; the paid plan starts at 5 USD a month. The visitor's address would reach the services only in a
    header the server cannot tell from a forged one without a secret shared with the Worker, Authenticated Origin Pulls or a Tunnel (Cloudflare's documentation on `CF-Connecting-IP` in
    Worker subrequests), so the faucet's five requests a day per address could be bypassed. The P2P ports would need a name of their own. And the page, deployed by Cloudflare at every merge,
    would no longer be built from the commit the server runs, as `deploy.sh` builds both (D-107) and the page reports it. WebSocket would pass through a Worker; that was not the obstacle. The
    page's data all come from the server, so serving its files elsewhere would not keep it working without the server.
  - The site and the docs read nothing from the server: Cloudflare serves them from its cache everywhere, they stay up when the server is down, and the server's bandwidth stays with the
    seed nodes and the RPC.
- **Costs and limitations**:
  - What a visitor's browser keeps for the page (the theme, the language, the data source, the wallet last connected, recent recipients) belongs to the old origin; at the new one the page
    starts without it and a browser wallet asks again to connect. No key is lost, as the page keeps none (`web/src/wallet/connect.ts`).
  - The former name keeps working only while its `A` record points at the server and Caddy renews its certificate; `--former none` ends its endpoints.
  - `signet.ksana.network` must stay DNS only: proxied, it would cut the P2P ports and the per-address limits.
- **Impact**: no consensus change. `signet/Caddyfile` (the former name), `signet/docker-compose.yml` (`KSANA_SIGNET_FORMER_DOMAIN` for the web service), `signet/.env.example`,
  `signet/server.sh` (`setup`'s third argument, `check_name`), `signet/deploy.sh` (`setup --former`), `signet/README.md` ("A new domain"), `docs/SIGNET.md` (the host named); PROGRESS M32
  step 11; CLAUDE.md.

## D-113 The project's site and docs at ksana.network: a static site generated in `site/`, served by a Cloudflare Worker with static assets only, in English and Chinese (2026-10-14, asked by the project owner after registering `ksana.network`; hosting per D-112; revises D-099's second point and CLAUDE.md's "Language"; a new directory with its own dependencies, no consensus change, M37)

- **Problem**: on 2026-10-14 the project owner registered `ksana.network` and asked for a landing page and docs on it, "not held back by tokens, to your own taste, until you like it yourself".
  The landing page has to say what Ksana is, why it exists and how it works; the docs have to take a developer from nothing to using the public signet (D-107, D-112): the web wallet, the
  JSON-RPC, the TypeScript SDK, `ksana-cli`, a node or an indexer of their own, and contracts. The project owner reads Chinese and the project writes English, so both languages are needed, while
  D-099 allows Chinese only in the web page's language packs. D-112 put the site on Cloudflare Workers, apart from the signet's server.
- **Decision**:
  1. **A static site generated in `site/`.** `site/build.mjs` (about 2 450 lines with `site/lib/` and the browser scripts, no framework) builds the landing page from its copy in
     `site/content/<language>/landing.mjs` and the docs from Markdown in `site/content/<language>/docs/`, with the order of the docs in `site/site.config.mjs` and the figures drawn by code in
     `site/lib/figures.mjs` (SVG for the landing page's dial, rails and supply curve, HTML for the docs' figures so that their labels stay text on a phone and in Chinese). The output is plain
     HTML and CSS with two small scripts (`site/src/js/site.js`: theme, search, copy buttons, code tabs, the docs menu; `site/src/js/landing.js`: the dial and the live strip); code is highlighted at
     build time. The dependencies, pinned exactly with the lockfile committed: markdown-it 14.3.2 and markdown-it-container 4.0.0 (Markdown and its `:::` blocks), shiki 4.4.3 (highlighting with
     TextMate grammars, so Rust, TypeScript and the shell read as in an editor), and for the deploy only wrangler 4.149.0 (the first version whose dependencies have no known advisory: 4.143.1 pulled
     sharp with GHSA-wq5f-xc86-pv6w; it needs Node 22, so `site/package.json` asks for 22 or later). None is in SPEC §12's table, as none of it is part of the chain.
     `node build.mjs --check` checks every internal link and anchor, that the two languages have the same pages with the same heading levels (anchors are shared by position), that no
     Chinese appears outside `site/content/zh/`, and that the Chinese heading font covers the Chinese headings.
  2. **Served by a Cloudflare Worker with static assets only** (`site/wrangler.jsonc`, the Worker `ksana-site`): Workers Builds, connected to the repository, runs `npm run check`
     and then `npx wrangler deploy` in `site/` at every push to `main` that changes `site/`; the check builds the site and stops the deploy when it fails, so a broken link never goes
     live (the build command is set in the dashboard, as Workers Builds ignores the `build` of `wrangler.jsonc`, which serves a deploy from a machine). The deploy attaches
     `ksana.network` and `www.ksana.network` as the Worker's custom domains (Cloudflare creates their records and certificates), and a redirect rule of the zone moves `www` to `ksana.network`. `html_handling` `auto-trailing-slash` answers `/docs/x/` from `docs/x/index.html`, `not_found_handling`
     `404-page` answers the rest with `404.html` and status 404, and `_headers` sets a content security policy (`default-src 'self'`; scripts from the site and the theme script's hash; connections
     to the site and `https://signet.ksana.network`), long caching for the files named by their content, and no framing. `workers_dev` is off, so the site has no second copy on `workers.dev`;
     the builds of other branches get preview URLs.
  3. **English and Chinese.** English is the source; the Chinese pages in `site/content/zh/` follow it page by page and heading by heading, with the project's glossary of terms. Chinese may
     appear there, besides the web page's language packs (D-099); nowhere else in the repository, which `--check` enforces for `site/`. The brand stays "Ksana" in both languages.
  4. **No third party.** The fonts are served by the site (SIL Open Font License, `site/src/fonts/OFL.txt`): Source Serif 4 for headings, IBM Plex Sans for text, IBM Plex Mono for code,
     and for Chinese headings a subset of Noto Serif SC SemiBold holding only the characters they use (`site/scripts/subset-zh-serif.py`; 85 KiB for the 489 characters of the first version); Chinese text uses the
     system's fonts. No analytics, cookies or external scripts (Cloudflare's Web Analytics, when its automatic setup is on for the zone, adds its beacon to every
     page; the policy blocks it, so the setup stays off). The only request to another origin is the landing page's live strip: while the page is visible, `ksana_getChainInfo` and
     `ksana_getBlock` of the tip from the signet's `/rpc` every 10 s (CORS for any origin, D-107); the strip stays hidden when they fail or the node is not the signet.
  5. **The docs follow the code.** They restate for users what `docs/SPEC.md`, `docs/ABI.md`, `docs/RPC.md` and `docs/SIGNET.md` define, checked against the code: where one of those
     documents and the code disagree, the pages follow the code. The outputs they show are real: the reads, faucet requests, transfers and the tutorial's contracts ran against the signet on
     2026-10-14, the contract code compiled and passed its tests locally, and the TypeScript examples type-check against the SDK. What needs a bitcoind holding the signet's chain (a node or an
     indexer joining the signet, path C and top-ups from `ksana-cli`) and the browser wallets follow the code and the repository's guides instead. The signet's addresses come from
     `site/site.config.mjs`.
  6. **What the pages leave out for now, and what they link** (the project owner, 2026-10-15). The pages say nothing of the block share's size, how long it lasts or whose account it
     pays (SPEC §8.6 and the code are unchanged): they name the block share account only where the order of a payout, the genesis config or `locked_fees` of `ksana_getAccount` needs it,
     and the landing page's supply figure draws the issuance alone. The top bar (from 360 px wide), the menu and the footer link the project's Discord server (`discord` in
     `site/site.config.mjs`). The Chinese pages link the signet's web page with `?lang=zh-CN`, as it opens in English otherwise (D-115); `--check` fails on a link without it.
- **Rationale**:
  - A generator of its own rather than a framework (Astro, VitePress, Docusaurus): three runtime dependencies instead of hundreds, nothing in the browser beyond two small scripts, and the
    design (the web wallet's colors, type and marks, WEB_DESIGN §1 and §2) drawn exactly, figures included. The cost is the generator's code, which is small and has no plugin interface to keep up.
  - A Worker with assets only, as D-112 asked: requests that only return static assets are free and unlimited on every plan (Cloudflare's documentation on the billing of Workers static
    assets), the site stays up when the signet's server does not, and the deploy follows `main` with nothing to run by hand.
  - Fonts and data from the site's own origin keep the policy strict and the visitor's address with Cloudflare alone.
- **Costs and limitations**:
  - The docs are written by hand: `--check` catches a broken link or anchor and a Chinese page out of step, not a stale fact. A change to the CLI, the RPC, the SDK or a parameter has to update
    the page that describes it.
  - While the repository is private, the steps that clone it (building `ksana-cli`, a node, an indexer or the SDK from source) work only for those with access; the SDK is not on npm and is
    installed from a packed tarball.
  - Changing a Chinese heading means running `site/scripts/subset-zh-serif.py` again (with the 14 MB source font, not committed); `--check` fails until it is run.
  - The images `site/src/img/og.png` and `apple-touch-icon.png` are drawn by `site/scripts/make-images.mjs` with a headless Chromium and committed.
  - The first deploy ran from the project owner's machine on 2026-10-15: `npx wrangler deploy` with an API token holding the account's Workers Scripts Edit and the zone's Workers
    Routes Edit, which creating the Worker and attaching its custom domains need. Connecting Workers Builds to the repository and the redirect rule for `www` take the dashboard once.
- **Impact**: no consensus change, nothing outside `site/` but the documents. `site/` (new: the generator, the content in English and Chinese, the fonts, the images, the Worker's
  configuration); CLAUDE.md ("Language": Chinese also in `site/content/zh/`; the site's commands and responsibilities); PROGRESS M37.

## D-114 A block is reported BTC-final once the BTC block that determined it has `BTC_FINALITY_CONFS` confirmations, and a BTC height the finality determination paused at is determined again when it resumes (2026-10-14, the recommendation of the review of how deep a BTC reorg must go to take back a BTC-final block; revises D-082 and SPEC §6.5, §10.3, §10.4, §11.2, §13, §13.1, §15, §16, `docs/RPC.md`; node and indexer reporting, the meaning of `finalized` and `btc_final`, and a fix of the determination's implementation; no change to block validity, the encodings or the rules of the fork choice and the determination, M38)

- **Problem**: D-082 promised that taking back a BTC-final block takes a BTC reorg of at least `BTC_FINALITY_CONFS` blocks ("Security"; SPEC §6.5, §10.4 "a reorg of at most
  `BTC_FINALITY_CONFS` blocks never takes back a BTC-final block", §13.1, §16), and node and indexer reported the determined tip as BTC-final the moment it was determined. The determination of
  block C at BTC height H rests on two things: the data of C's closure, landed at or below H − `BTC_FINALITY_CONFS` + 1 and so `BTC_FINALITY_CONFS` blocks deep at H, and no candidate with a
  larger fork-choice key having connected by H (`conn ≤ H`, D-082 item 2), which is only as deep as the BTC block at H. A BTC reorg revokes what was determined at the BTC blocks it replaces and
  determines it again along the new branch (D-082 item 4). When the new branch carries, at or below H, the headers of a heavier chain forking below C (an attacker's settlements, confirmed first by
  the miner making the reorg), the determination made again at H takes that chain, and C leaves the finalized chain for good: right after the determination a reorg of the one BTC block H does it, and
  one more block for each BTC block since; the finalized tip, the floor of every main chain, moves back with it. Without such a chain the determination made again is the same unless the reorg removes
  C's data, `BTC_FINALITY_CONFS` blocks right after the determination. So the code did what the rule says, and the property stated for the rule did not hold. The unit test
  `a_btc_reorg_revokes_the_decisions_made_on_the_orphaned_blocks` already showed it with a 2-block reorg. The two audits of 2026-10-06 checked that the rollback is correct and that evaluating
  incrementally equals evaluating from scratch, and the reviews of D-104 repeated the depth of D-082; none asked how deep a reorg has to reach. Against someone with a heavier fast-lane chain (a rented
  majority is cheap, §16) who can get one BTC block replaced (within reach of a large pool, or one paid to do it), a block just reported BTC-final was protected by 1 BTC block, not 6. The notes of the
  site's docs (M37) raised it. Fast-lane confirmations, pruning (its root is determined at least `PRUNE_BTC_MARGIN` BTC blocks back)
  and D-104's retreat (a reorg of at most `BTC_FINALITY_CONFS` blocks never reaches the anchor of a final block) are not affected.
  - **A second gap, in the code**, found while testing the first: a BTC height the determination paused at (the data status or the verdict of a block not known yet, D-082 item 3, D-101) was not
    recorded as evaluated, while the connections, determinations and exclusions made there before the pause were kept. When a BTC reorg then replaced only the BTC block at that height, the evaluation
    resumed there without revoking them, as it goes back only to the highest evaluated height still on the best chain: what was determined on the replaced block stayed, against D-082 item 4 and
    unlike an evaluation from scratch, so two nodes, or a node and an indexer, that saw the BTC chain at different moments could end on different finalized tips.
- **Decision**:
  1. **Report BTC-final once the BTC block of the determination has `BTC_FINALITY_CONFS` confirmations** (the review's recommendation (b)). The BTC-final tip is the highest finalized block whose
     determination was made at a BTC block on the best chain with `BTC_FINALITY_CONFS` confirmations (`Decision.btc_height` / `btc_hash`, which node and indexer already keep); the genesis block
     and the pruning root (determined at least `PRUNE_BTC_MARGIN` BTC blocks back, D-097) are always BTC-final. `ksana_btc::finality::Finality::btc_final`, `BtcFinal`.
  2. **What was reported stays while its determination stands**: the BTC-final tip reported before is kept when it is higher and its determination still stands, made at the same BTC block, which
     is still on the best chain and evaluated. During a BTC reorg the best chain can be shorter for a while (a sync between the reorg and the new branch, a snapshot published part way through, an
     operator's `invalidateblock`); measured against such a tip, a block reported would no longer be deep enough and would be taken back while no BTC block of its determination was replaced. So a block
     reported is taken back only by a BTC reorg replacing the BTC block of its determination, which had `BTC_FINALITY_CONFS` confirmations when it was reported: at least `BTC_FINALITY_CONFS` blocks.
     The determination made again then takes it off the finalized chain only when the new branch carries a heavier chain forking below it, or when the reorg also removes its data, at least
     `2 × BTC_FINALITY_CONFS − 1` blocks (the data landed `BTC_FINALITY_CONFS` − 1 blocks below the BTC block of the determination). The tip reported before is kept in memory only: after a restart,
     node and indexer report from the determinations made at confirmed BTC blocks.
  3. **An alert when a reported block is taken back**: node and indexer log a highest-level alert naming the block and the new BTC-final tip (`Finality::taken_back`). A reorg revoking
     determinations not yet reported is logged without an alert: a natural 1-block reorg revokes the latest determinations and makes them again in the same round, and D-082's alert ("a BTC reorg undid
     the finality decision") fired each time.
  4. **The RPC keeps its names, with the reported meaning**: `finalized` of `ksana_getChainInfo` and `indexer_status` is the BTC-final tip, and `btc_final` of `ksana_getOpStatus`,
     `ksana_getSettlement` and the op listings (D-111) means that the main-chain block that accepted the op or block is on the finalized chain at or below the BTC-final tip. In the code: the
     node's `Chain::btc_final` / `is_btc_final` and the indexer's `IndexerStatus.btc_final_height` / `btc_final_hash`; `Chain::finalized` / `is_final` and `IndexerStatus.finalized_height` /
     `finalized_hash` stay the determined tip.
  5. **The determination does not wait**: the finalized tip of SPEC §6.5 is still the floor of the main chain (§6.5 item 5), the selected parent of the verdicts (item 6), what pruning measures its
     root from (D-097), where the kept states start (D-101) and what the indexer executes without waiting for `SETTLEMENT_DEPTH` (§10.3).
  6. **A paused height is determined again from the start**: every evaluation first revokes what was connected, determined and excluded above the highest evaluated height, with the cached
     closures that landed above it, and evaluates the next height again on the BTC block now there. A determination revoked this way because its BTC block was replaced is reported as a rollback; an
     exclusion made again at the same height is not reported again. Without a reorg, the height is determined again from the same inputs to the same result.
- **Not consensus-visible** (items 1 to 5): no consensus code reads what is reported; it is not hashed, signed, sent to peers or put on BTC. Block validity, the encodings, the state roots, the fork
  choice, the determination and everything that follows the determined tip (the floor of the main chain, the verdicts, the pruning point, the kept states, the indexer's execution) are unchanged, and
  so are settlement and the waits of path B (the settled closure, `SETTLEMENT_TIMEOUT`) and path C (forced inclusion by BTC confirmations), which never read BTC-final. Two nodes can report different
  BTC-final tips for a while (one that kept its tip through a shorter BTC chain, one restarted then), as they can differ in what they have synced. Item 6 changes the determined tip only where the code
  departed from D-082 item 4 (a reorg replacing only the BTC block a pause stopped at), toward the result of an evaluation from scratch, which every node and indexer gets alike: it is a fix of the
  implementation of a consensus rule, not a change of the rule, and it goes through the review a consensus-related change gets.
- **Rationale**:
  - The project owner chose the 6-block property (D-082, SPEC §13.1). Restoring it where it is reported keeps the determination as it is, so nothing needs a restart, and keeps what D-091 and D-100
    rely on: closing the window earlier instead (counting only the headers connected `BTC_FINALITY_CONFS` blocks before the determination) would bring back the lighter chain rushed onto BTC, and making the
    determination itself wait would delay the floor of the main chain, pruning and the indexer for nothing, as a BTC reorg takes them back anyway.
  - The review's lighter variant (giving the BTC height of the determination in the RPC and leaving the depth to applications by amount) would leave every application to get it right, and the wallet
    page shows BTC-final as the node reports it.
- **Costs and limitations**:
  - BTC-final comes `BTC_FINALITY_CONFS` − 1 BTC blocks later: about 110 minutes after the op on mainnet instead of about 60 (the data lands with the next BTC block or so, then 10 confirmations instead
    of 5); about 2 minutes on devnet, with a BTC block every 10 seconds.
  - A block determined but not yet reported can still leave the finalized chain with a reorg of 1 BTC block and a heavier chain; that is the fork choice at work, as before, and nothing is promised for it.
  - A reorg of at least `BTC_FINALITY_CONFS` blocks reaching the BTC block of a reported block's determination can still take the block back, as the 6-block property allows: with a heavier
    chain on its new branch the block leaves the finalized chain, and without one it is determined again and reported again once the new BTC block of its determination has
    `BTC_FINALITY_CONFS` confirmations, at once when the new branch is already that long. Node and indexer alert whenever a block they reported stops being reported.
  - The web page shows `finalized` and `btc_final` as the node reports them. Its BTC-final step, its estimates of the wait and its simulator count to the data's `2 × BTC_FINALITY_CONFS − 1`-th
    confirmation (`BTC_FINAL_DATA_CONFS` in `web/src/chain/params.ts`), the earliest a block can be reported BTC-final, as the RPC does not give the BTC height of a determination; its copy gives
    about 110 minutes on mainnet (a follow-up in M38). Its BTC-final height never goes above `finalized` (read when BTC advances and when the statuses give more), so a report the node takes
    back stops showing without a reload and the statuses above it are asked again; a step with all its confirmations that the node has not reported yet says it waits for the node's report; and a BTC
    block's seal in the two-lane view, with the vermilion ticks of its page, waits until the blocks whose data it carries are BTC-final (the review of a follow-up in M38,
    optional items 1, 2 and 4). The time a final step gives is still that of the data's 11th confirmation, earlier than the report when the
    determination waited (its item 3): the RPC gives no time of a report.
  - **Revised by D-116** (web page only): the page counts the data's confirmations to `BTC_FINALITY_CONFS` as a milestone of its own and gives BTC-final a step that counts nothing; its
    estimates and its simulator still reach BTC-final at the data's `BTC_FINAL_DATA_CONFS`-th confirmation.
- **Impact**: node and indexer reporting; no restart. Code: `ksana-btc` (`finality.rs`: `BtcFinal`, `Finality::btc_final`, `Finality::taken_back`, the paused height evaluated again in
  `Finality::evaluate`, `rollback` choosing the candidate again only when it revoked something), `ksana-node` (`chain.rs`: `FinalityState::reported`, `Chain::btc_final`, `Chain::is_btc_final`, the
  alert; `rpc.rs`: `ksana_getChainInfo`, `ksana_getOpStatus`, `ksana_getSettlement` and the op listings report from them), `ksana-indexer` (`IndexerStatus.btc_final_height` / `btc_final_hash`,
  `indexer_status.finalized`, the alert), the doc comments of `ksana-rpc` and the TS SDK. Tests:
  - `finality.rs`: `a_decision_is_reported_btc_final_once_the_btc_block_that_made_it_is_deep` (the review's case: right after a1 is determined, a reorg of just the deciding BTC block whose new block
    carries a heavier chain forking below a1 revokes a1 for good, and nothing reported is taken back; once a1 is reported, a reorg of `BTC_FINALITY_CONFS` − 1 blocks takes nothing back, also onto a
    branch shorter for now; one of `BTC_FINALITY_CONFS` blocks with the heavier chain takes a1 back; one without it, onto a branch as long, takes nothing back until it removes a1's data, `2 × BTC_FINALITY_CONFS − 1`
    blocks, and onto a branch shorter for now takes a1 back until the new BTC block of its determination has `BTC_FINALITY_CONFS` confirmations);
    `a_reorg_of_the_height_the_evaluation_paused_at_revokes_what_was_decided_there` (item 6; fails without it); the property test
    `incremental_evaluation_through_pauses_and_reorgs_matches_evaluation_from_scratch` (256 cases of random DAGs with invalid blocks, and verdicts and data statuses that arrive late, through BTC
    reorgs onto branches carrying the headers above the fork point at other heights and shorter for a while or not, some of them from the height a pause stopped at and some down to the genesis
    block's anchor: after every step incremental evaluation equals evaluation from scratch, and no reorg of fewer than `BTC_FINALITY_CONFS` blocks below the highest tip yet takes back the tip
    reported; without item 6 it failed in 6 of 6 runs. The pauses for data statuses and the comparison after every step follow the review of M38's first change; before
    them it failed in 5 of 6 runs).
  - `crates/ksana-indexer/tests/deep_reorg.rs`: `a_reorg_of_the_btc_block_that_decided_a_block_with_a_heavier_chain` (a producing node, an indexer and both RPCs, the heavier chain's settlements
    held back in the BTC mempool and confirmed first by the new branch: a reorg of 1 block right after the determination takes a1 off the finalized chain, which neither node nor indexer had reported,
    where before this change both had reported it, and B's chain and its op become final instead; once a1 is reported, a reorg of `BTC_FINALITY_CONFS` − 1 blocks, with node and indexer synced midway
    on the shorter branch, takes nothing back; one of `BTC_FINALITY_CONFS` blocks takes a1 back); `assert_matches_node`, which every indexer test against a node calls, also checks that the indexer
    and the node report the same BTC-final tip.
  - Regtest: `m3_regtest.rs` mines the confirmations of the BTC block of the determination too before it waits for `btc_final`.
  - Web page: the simulator's invariants check that the BTC-final tip's closure has `2 × BTC_FINALITY_CONFS − 1` confirmations and the next settled block's fewer (`engine.test.ts`), and
    `views.test.ts` follows a path B op to one confirmation short of that, not yet BTC-final, and then to it; with the simulator's old count 7 web tests fail. After the review of the follow-up: in
    `chain.test.ts` a synthetic node takes a report back, and the BTC-final height follows in the sync where BTC moved, the two statuses above it are asked again in the next, and the tip reported
    again shows in the sync whose statuses give it; `views.test.ts` holds the simulator's BTC-final height below the op's block with its confirmations all there (the steps wait and say so, the BTC
    block has no seal) and checks the seal against a stub chain (a parallel block accepted later holds it back, a block no chain block accepts and a path C op do not); with the rules before, 5 of
    these tests fail.

## D-115 The web page opens in English whatever the browser's languages (2026-10-15, asked by the project owner; revises D-071's choice of language; web page only, no consensus change, M36)

- **Problem**: since D-071 the page took its language from `?lang=`, then the language last chosen in its menu, then the browser's languages, and English only when none of them was
  supported, so a phone set to Chinese, Japanese, Korean or Spanish opened the explorer and the wallet in that language. The project owner asked for English as the default.
- **Decision**: the page opens in English unless `?lang=` in the address bar or a language chosen in the menu (top bar, footer, settings) says otherwise; the browser's languages are
  no longer read. As before, a choice made in the menu is kept on that device (`localStorage` `ksana:locale`) and takes `?lang=` out of the address bar, and a link's `?lang=` applies
  without being kept. English now ships in the bundle and Simplified Chinese is loaded on demand like the other languages, so the first load of the default waits for no language
  file; Simplified Chinese stays the source language whose structure is the `Messages` type.
- **Rationale**: one language for everyone who has not chosen, the language of the project's public face; the menu is one tap away on every page and remembers the choice.
- **Costs and limitations**: a reader whose browser is set to another language sees English first and has to choose theirs once on each device. A link meant for readers of another
  language can carry `?lang=` (`?lang=zh-CN`, `?lang=ja`); the project's site links to the signet's page without it (`site/site.config.mjs`).
- **Impact**: `web/src/i18n/index.ts` (`detectLocale`, the bundled language), its unit test; `web/README.md`, `docs/WEB_DESIGN.md` §8.4, D-071's heading, PROGRESS M36.

## D-116 The web page counts the data's Bitcoin confirmations to 6 and shows BTC-final as a seal that follows them without a count of its own (2026-10-15, chosen by the project owner; how the web page presents D-114; web page only, no change to the node, the indexer, the RPC, the SDK or consensus, M38)

- **Problem**: since D-114 a block is reported BTC-final once the BTC block of its determination has `BTC_FINALITY_CONFS` confirmations, at the data's `2 × BTC_FINALITY_CONFS − 1`-th, its 11th,
  at the earliest. The web page counted its BTC-final step to 11 (11 tick cells, "10 / 11 confirmations"), its notifications said 11 BTC confirmations, its hover text said the related transactions
  have 11, its console mined 11 BTC blocks at a time. The project owner found 11 confirmations a poor impression beside the 6 a Bitcoin wallet counts to, and asked whether they are needed. They are
  not a mathematical necessity: reporting at the data's (5 + k)-th confirmation leaves a reported block to a reorg of k BTC blocks together with a heavier fast-lane chain, so 6, 7 or 8 would cost
  the 6-block property of D-082. The project owner chose to keep D-114's depth and change how the
  page presents it.
- **Decision**:
  - A stage between written to Bitcoin and BTC-final, `deep`: the chain block accepting the block is in the settled closure and the closure has `BTC_FINALITY_CONFS` BTC confirmations
    (`blockStage`). The progress of an op and of a block gets a step for it, "6 BTC confirmations", counting the closure's confirmations in 6 tick cells: the milestone the page shows first, with
    its own mark (the ink square with a check), hover text and notification.
  - BTC-final is the seal that follows it, with no tick cells: before the six confirmations its note says it comes after them; after them it is in progress, "Securing", with the time it is
    expected; with its BTC blocks all there and no report yet it waits for the node's report (D-114); done, it says what taking it back would take, a Bitcoin reorg of `BTC_FINALITY_CONFS`
    blocks or more. The stage itself is still the node's report (D-114).
  - The page never counts to 11 or shows the number: a BTC block's page counts its confirmations to 6, turning vermilion once its data is BTC-final; the simulator's console mines 6 BTC blocks at
    a time; the hover text of BTC-final says what taking it back would take; the Verify page gives its condition in D-114's two parts, the data's 6 confirmations and the 6 of the BTC block that
    decided it; the Receive page and the Send page's large-amount warning give the wait as a time; the Send page's summary gives the time to the six confirmations and the time to BTC-final.
  - Large amounts still wait for BTC-final: the six-confirmation stage's hover text says so, and the Receive page's rule, the operation page's banner and the wallet's reminders still wait for it.
  - The notifications about an op sent from the wallet announce the six confirmations and BTC-final like the stages before them, on the way up only (`opMilestone`): a BTC reorg
    taking a stage back is not announced, and the stage reached again is. When the node no longer reports the op BTC-final, a warning says so, whatever stage the op falls to,
    and does not time out. It says what the page saw, not why ("BTC final withdrawn": the node no longer reports it as BTC final): the node takes a report back after a BTC reorg
    reaching the BTC block that decided it (D-114, the case the node and the indexer log as an alert), and reports the root of its finality decision until its first decision
    after a start (it keeps the tip it reported in memory only), which shows when the first BTC sync after a restart fails or on a resync from scratch (on the signet the node
    starts once bitcoind has synced): the page cannot tell the two apart (the review of D-116). An op whose execution failed is announced
    once, as failed, when it is included (before, as fast-confirmed), and nothing after it: the notifications follow the op's mark as the lists show it (`opStage`), a failed
    op's its failure whatever its block's stage, so its block's Bitcoin stages, BTC-final and their taking back, which would read as the failed transfer's or call's, are not
    announced for it (the review of D-116). An op that falls from BTC-final straight to its end, executed again in another block after such a reorg and failed there (or
    skipped, or dropped), gets the warning and then its end (the review of D-116, on its fixes).
- **Rationale**: six confirmations are what a Bitcoin user counts, about an hour after the op on mainnet; giving them a milestone of their own puts the familiar number first, and BTC-final keeps its
  meaning and its place as the last step. The milestone is the data's depth as the page counts it, not a node's state: a node decides a block final at that depth only when its fast-lane chain is
  the heaviest on Bitcoin, and the RPC does not say when it has. A state reported by the node ("decided", option D of the trade-offs) would be more exact but changes the node's RPC, the SDK and
  the signet's nodes.
- **Costs and limitations**: the wait for BTC-final is unchanged: about 110 minutes after the op on mainnet and on the signet, about 2 on devnet, and the page still says so where it gives a time.
  A block at six confirmations is not final yet: a reorg of 1 BTC block with a heavier fast-lane chain can still take it off the final chain, which is why the page sends large amounts on to
  BTC-final. A settled block whose closure the page does not know stays at written to Bitcoin until it is reported BTC-final. With a node the page knows a closure only once its cache
  reaches the BTC-final boundary: a parent outside the cache counts as final only then, and never one that a chain block above the BTC-final tip merges (the sync fetches it after that
  block), as otherwise it may be a block not yet final whose data landed after the cached ones' (a settlement bumped or rescued late), and counting from the cached blocks alone would show
  the six confirmations early. So while a page load backfills, at 40 blocks a sync (about a minute and a half for the stretch's average, up to about four minutes for its
  longest), and while more blocks are not final than the backfill goes down to, no block shows the six confirmations or counts toward them. The backfill goes down to 3 600 blocks
  below the tip, five hours of 5-second blocks (`MAX_DEPTH`; 1 500 before the first review of D-116): with a block every 5 seconds the stretch not yet final spans the landing of the data
  and its 11 confirmations, about 12 BTC blocks, and is some 1 400 blocks on average, longer than 1 500 about a third of the time and longer than 3 600 less than once in 10 000
  (BTC's block intervals taken as exponential, an estimate rather than a measurement). The first load costs the same, and so does a stretch of up to 1 500 blocks; a longer one costs
  more as it grows, at 3 600 blocks 2.1 to 2.4 times the memory, 2.5 to 3 times the page's own time a sync and, with an op in every block, 2.3 times the statuses asked again every
  minute (3 600 against 1 549, a `ksana_getSettlement` each: the node's load), by the measures of the review of D-116. Where calls go out in batches those statuses go a batch's worth
  at a time: 6 at a time, the minute's refresh took 602 round trips at that depth, holding back the next sync and with it the tip and the user's own ops, where it takes 59 (the review
  of D-116); a node that takes no batch still gets them 6 at a time. A BTC reorg can take a stage back, which only BTC-final's warning announces.
- **Impact**: `web/` only; nothing to restart, the signet's web page is rebuilt. `chain/views.ts` (the stage `deep`, `blockStage`, the steps of `opProgress` and `blockProgress`, `StepView.mark`,
  `opMilestone`, which follows `opStage`, and `opNews`, what `OpWatcher` announces), `chain/rpc/chain.ts` (a parent outside the cache counts as final only once the cache reaches the BTC-final boundary, and not when a chain block above that boundary
  merges it; the backfill's `MAX_DEPTH`; the statuses asked again a batch's worth at a time where calls go out in batches), `components/Status.tsx` (the mark), `components/Tracker.tsx` (a done step's mark), `components/RailView.tsx` (the hover of a BTC block with six
  confirmations), `app/Shell.tsx` (the notifications, the warning when BTC-final is taken back, and the console's mine button), `app/prefs.ts` (`Toast.mark`), the pages Send, Receive, Verify, BtcBlock and BlockDetail, the six language packs, the styles; `docs/WEB_DESIGN.md`,
  `docs/SIGNET.md`. The two Han characters it adds to the Simplified Chinese copy are in step notes, hover text and notifications, never in a heading, so the heading font's subset
  (`web/scripts/subset-font.py`) stays as it is. The project's site (M37) says the same. Tests: `views.test.ts` follows a path B
  op through the six confirmations (the step's cells and note, the stage `deep`, the BTC-final step in progress with no cells) to BTC-final, and a path C op's and a block's steps;
  `rpc/chain.test.ts` replays the recorded node with its BTC tip moved up (the six confirmations come with the closure's sixth, and 30 BTC blocks later BTC-final is still only what the node
  reports), with the blocks below a cache of the first sync's window landing after the window's own (no closure, no six confirmations), and with a chain block's merged blocks not
  fetched yet (no closure from that block up), and a synthetic node's chain backfilled to its BTC-final tip 3 000 blocks below the tip, and to 3 600 below the tip when that
  tip is lower, its 1 000 statuses not yet final asked again in 16 requests where calls go out in batches (167 six at a time), and a synthetic node that restarts, whose report
  goes back with no BTC reorg, under a warning that names no count, no Bitcoin and no reorg in any language (in Chinese, Japanese and Korean, no run of letters shared with the
  language's own note of BTC-final, which names one); `views.test.ts` runs `opMilestone` along stages: each milestone once, the failure at inclusion, no announcement on the way down, BTC-final taken back from any stage,
  an inclusion in another block announced again, an op falling from BTC-final straight to its end, and follows a failed transfer and one that succeeds in the simulator to
  BTC-final through `opNews`, as the watcher asks it (the failed one announced once, as failed; with the progress's stage in place of the op's mark it is announced four times, never as failed),
  and the two ops `opNews` holds back: the one long BTC-final whose block left the page's cache, which read without its block would be announced as taken back, and an amount in
  an asset the node has yet to name, no news until it is named, then announced by its name.

## D-117 Outside a race the node's BTC transactions pay the floor while bitcoind's mempool holds at most half a block (2026-10-18, from the project owner's report of the signet's settlement fees, with the review of the change; revises D-102 and SPEC §7.2, §7.7, §14, `docs/RPC.md`; local policy, no consensus change, M32)

- **Problem**:
  - The project owner reported on 2026-10-18 that the signet's settlements kept paying the highest fee rate: the page showed a settlement fee rate of 100 sat/vB (`ksana_estimateFee`) and
    offered path C at 33 500 sats. D-107 ("Signet's fee estimate") has the record: from 2026-10-16 every settlement pair of the seed nodes paid the cap of 100 sat/vB, about 37 000 sats a pair and
    0.022 sBTC a day for the network, while signet's blocks took transactions at 1 sat/vB. The seed nodes' cap went down to 5 the same day as a stop-gap.
  - Cause: D-102 pays bitcoind's `estimatesmartfee` within the floor and the cap and never asks whether the next block has room. The server's bitcoind on 2026-10-18 at 19:28 UTC (BTC height
    326 930): `getmempoolinfo` 64 transactions, 314 541 vB (`bytes`), `mempoolminfee` 0.1 sat/vB, only 2 of the transactions above 50 sat/vB; `estimatesmartfee` 100.045 sat/vB for 1 and 6
    blocks, 21.1 for 24; `estimaterawfee` for 3 and 6 blocks: the short horizon 100.045, from its passing range of 97.2 to 202.1 sat/vB with 30.6 transactions confirmed within the target, the
    range below it, 17.6 to 97.2 sat/vB, failing with 13.5 within the target and 5 still waiting; the medium horizon 18.2; the long horizon 0.1, from its range of 0 to 0.99 sat/vB with 328.5 of 330.0
    within the target.
  - Reading (inferred from these figures; bitcoind's per-transaction data was not inspected): the short horizon, which answers targets up to 12 blocks, has few samples on signet. The range that
    passes is the seed nodes' own settlements at the cap, which confirm in the next block; below it the ranges fail, as a transaction waiting in this bitcoind's mempool longer than the target
    counts against the range it pays, and some of those waiting here are ones signet's miners leave out. Once the estimate reached the cap, the seed nodes' payments at the cap kept it there.
- **Decision**:
  a. Outside a race with a lighter competing chain (D-091), a new BTC transaction of the node (a settlement, a rescue, a republish, a funding split) pays the floor `--settlement-feerate`, or
     bitcoind's `mempoolminfee` (rounded up to whole sat/vB) when that is higher, up to the cap, while bitcoind's mempool holds at most `ROOMY_MEMPOOL_VSIZE` = 500 000 vB: half of a block's
     4 000 000 weight units, by `getmempoolinfo`'s `bytes`, the sum of the waiting transactions' virtual sizes. The next block then has room for every transaction waiting, and one paying the
     relay minimum is taken whatever the estimate says. With more waiting, while bitcoind is still loading its mempool after a restart (`loaded` false) or when `getmempoolinfo` fails, the estimate
     of D-102 as before.
  b. The CPFP bump of D-102 item c keeps the estimate: a pair this node sent that has waited `BUMP_AFTER` BTC blocks and pays less than the estimated rate is bumped to it, whether or not the
     mempool has room. A pair the floor left waiting is a sign the room was not there for it (miners leaving it out, a miner seeing a fuller mempool than this bitcoind), and the bump is the way out.
     So that the way out does not depend on the estimate, which may be at or below the pair's rate (added after the review of the change): once a pair has waited `BUMP_AFTER` BTC blocks through
     rounds that all found room (outside a race), the bump's rate is at least the pair's own rate doubled for every such BTC block from the `BUMP_AFTER`-th on (twice it after 2 blocks, four
     times after 3), within the floor and the cap (`FeePolicy::escalated`), and each rise replaces the child (D-102 item c). A round without room starts the count again, so that blocks that had
     no space for the pair do not count against its rate.
  c. While racing a competing chain (D-102 item b) nothing changes: the next-block estimate, bumps at once.
  d. `ksana_estimateFee`'s `feerate_sat_vb` is the rate of item a (outside a race), and the web page's path C and top-up suggestions follow it.
  e. `BtcClient::mempool_info` (`getmempoolinfo`'s `bytes` and `mempoolminfee`, converted to sat/vB rounding up without floating point; `None` while `loaded` is false), `MempoolInfo`,
     `FeePolicy::room_rate`, `FeePolicy::estimated_rate` and `FeePolicy::escalated`; `MockBtc::set_mempool_info`, which tells nothing by default, so the earlier tests run on D-102's estimate
     as if the mempool were full.
- **Rationale**:
  - The mempool's size says whether the fee matters for the next block; the estimate looks back at what confirmed, and with few samples the operator's own payments hold it up. mempool.space
    suggests the minimum fee for the next block in the same case: its fee API gives a projected block of at most 500 000 vB the mempool's minimum, at least 1 sat/vB (`optimizeMedianFee`
    in its backend's `fee-api.ts`).
  - Half a block rather than a whole one leaves room for transactions this bitcoind has not seen and for those arriving before the next block.
  - New transactions pay the floor, and the bump after `BUMP_AFTER` BTC blocks is the way out of a wrong reading of the room. A bump to the estimate alone is no way out when the estimate is at
    or below the pair's rate: with no estimate (a new data directory, `-blocksonly`), and once this node's own payments at the floor have brought the estimate down, the case this decision aims
    for (the review of the change). The doubling of item b depends only on the pair's own rate and the blocks it waited with room: a pair the room misjudged misses `BUMP_AFTER` BTC blocks, and one
    more for every doubling past the first that the miners' rate needs, unless that rate is above the cap, past which nothing more is paid (D-102). Doubling every block takes a pair from the
    floor of 2 sat/vB to the default cap of 100 in six doublings, and each child replaces the one before, so the pair pays only for the last.
  - Races keep the estimate: they are rare, and landing in the next block is what they pay for.
  - `getmempoolinfo` is one cheap call, unlike listing the mempool, and `ksana_estimateFee` is a public method that asks for the rate on every call.
  - Paying the floor also gives the estimator samples at the floor, so where the node's own transactions were most of its data the estimate comes down as well.
- **Limitations**:
  - Transactions waiting in this bitcoind's mempool that miners never take count against the room: while they hold more than half a block the node pays the estimate, and the seed nodes' cap of
    5 sat/vB (D-107) stays for that case.
  - A miner whose mempool is fuller than this bitcoind's (a poorly connected bitcoind; one under `-blocksonly`, whose mempool holds only this node's own transactions and so always reads as
    roomy, and which has no estimate either, inferred) can leave a pair at the floor waiting until the bump of item b after `BUMP_AFTER` BTC blocks; without an estimate, the doubling bumps it.
  - Between half a block and a whole one, mempool.space scales its suggestion up toward the projected block's median; the node takes the estimate there.
  - Other nodes judge a waiting pair by their own rate and count no CPFP child (D-102 item d and its limitations): a node still running D-102, or a node on this decision whose own mempool holds
    more than half a block, is still loading or has a higher `mempoolminfee`, may judge a pair at the floor by a higher rate. The pair then starts that node's rescue order, whose first node,
    the producer of the next main-chain block, rescues once one BTC block has left the pair out (`rescue_delay`), a block before this node's own bump. With room the pair confirms in that block
    and nothing is rescued; otherwise a second pair goes out and both can confirm: an extra rescue costs fees, not liveness (D-102). On the signet the two seed nodes share one bitcoind and
    judge alike; on mainnet, nodes seeing different mempools is the normal case (the review of the change).
  - P2A pinning (D-102's limitations) now also holds a pair the room misjudged: when someone has attached a large low-rate child to the reveal's P2A output first, this node's child is refused
    every round, and the pair waits for other nodes, which, with room, judge it at the floor and hold their rescues back for `CLAIM_PATIENCE` BTC blocks, longer than the settlement backlog limit
    allows (the review of the change). Closed with D-102's by D-131 (2026-10-28).
  - The relay probe (`crates/ksana-btc/examples/relay_probe.rs`) keeps its automatic rate, bitcoind's estimate plus 1 (D-107): it is run by hand, and `--feerate` sets its rate.
  - Before mainnet (the review of the change, SPEC §14; the signet's margins do not need them):
    - The bump comes late for mainnet. A pair the room misjudged lands in the third BTC block after it was sent at the earliest, while under steady path B traffic with settlement batching a
      pair has 3 BTC blocks on mainnet before the settlement backlog limit (`SETTLEMENT_TIMEOUT` less `ANCHOR_DEPTH`, less the wait of the pair in flight before it; on signet 4 after a pair
      that landed in the next block, 3 after a slow one). A pair sent at the room rate is to be bumped after one missed BTC block, to the next-block estimate.
    - The room has no memory. It is read afresh for every new transaction, so under a steady mismatch (miners taking nothing below a rate above the floor while this bitcoind's mempool looks
      roomy, or block space sold outside the public mempool) every pair waits for its bump. A pair the room misjudged is to make the node pay the estimate for some BTC blocks (6, say) before
      it reads the room again.
    - The reading is biased low. It is usually taken right after a BTC block, when the mempool is smallest, as batching sends the next pair once the one in flight lands; block space sold out
      of band never shows in it; and a bitcoind restarted without its `mempool.dat` (a new data directory, a crash, `-persistmempool=0`) reads as roomy for a long time, as its peers do not
      announce again what they announced before (inferred from Bitcoin Core's behaviour; on the signet a normal deploy keeps the file, bitcoind having two minutes to stop). The reading is to
      be paired with the memory above or with what the last block took (`getblockstats`' `total_weight` and `minfeerate`).
    - The doubling reads every BTC block that leaves a pair out as a price signal, so a pair left out for another reason climbs toward the cap all the same: a block found before the pair reached
      its miner, or a pool that filters envelope transactions (none is known on the signet, where the relay probe found both data carriers mined, D-107). Only the last child is paid and the cap
      bounds it, but with mainnet's default cap of 100 a pair of about 500 vB with its child can end up paying about 50 000 sats after six doublings. It is to be weighed with the earlier bump
      above, which would bump more often (the review of the change, on its fixes).
- **Impact**: local policy only, no new flags; `BtcClient::mempool_info`, `MempoolInfo`, `producer::ROOMY_MEMPOOL_VSIZE`, `FeePolicy::room_rate`, `estimated_rate` and `escalated`;
  `Rescuer::run_once` bumps at the estimated rate, and `Rescuer::bump` at least at the escalated one after `BUMP_AFTER` BTC blocks with room. Tests: `ksana-btc` (`getmempoolinfo` parsed, still
  loading, malformed; the mock's mempool info); node (the fee policy with and without room, in a race, up to the cap, without bitcoind; the escalated rate within the floor and the cap; a
  settlement paying the floor while the mempool has room, left waiting by miners taking nothing below 5 sat/vB, bumped to the estimate of 10 after `BUMP_AFTER` BTC blocks and landed; one
  with no estimate and miners taking nothing below 3, bumped to twice its rate and landed; one bumped first after `BUMP_AFTER` + 1 BTC blocks, to four times its rate; one that waited without
  room first, bumped only after `BUMP_AFTER` BTC blocks with room; a rescue paying the floor while the mempool has room; a republish racing a competing chain paying the next-block estimate
  although the mempool has room; `ksana_estimateFee` following both). Documentation: SPEC's version history, §7.2, §7.7, §14, `docs/RPC.md`, CLAUDE.md, PROGRESS M32 step 14, the seed
  nodes' cap in `signet/README.md`, `signet/ksana-signet.sh` and `docs/SIGNET.md`, and the project's site (`run-a-node`, `parameters`, `mining` and `security`, in English and Chinese),
  whose pages also gain the seed nodes' cap of 5 sat/vB (D-107).

## D-118 The real-node contract playground keeps a write-ahead journal and offers explicit local nonce release (2026-10-19, from the review of the playground's first version; web only, no consensus change, M39; the playground moved to a page of its own with D-119, which keeps this journal)

- **Scope**: `#/playground` connects to real devnet or signet nodes. Deploy and Call are wallet-signed path B operations, with KSA as the only attached asset; `ksana_call` is a read-only query. The simulator and mainnet are not supported. No compiler service, private-key storage or automatic re-signing/broadcast retry is introduced.
- **Decision**: before broadcasting, persist the operation ID, chain/account, nonce, endpoint fingerprint, target and state, but never signatures, arguments or WASM bytes. One Web Lock per chain/account coordinates preparation, submission, recovery and status updates across tabs and endpoints. Signed submissions bypass the batching transport, whose read fallback may replay requests. Recheck the wallet/network, nonce, fee and balance around signing. An unknown result blocks further writes at that nonce.
- **Recovery**: an explicit `NOT_FOUND` may expose a release action. After a confirmation warning that this is not cancellation and the old op may still win, recheck the node, wallet/account and current nonce under the same lock. Release only that row, retaining its ID and ignoring only its matching stale local-pool record, never unrelated operations. Transport errors cannot authorize release. Nothing is signed or sent. A late pending report does not undo the local release; actual inclusion can still update its status.
- **Retention**: rows below the freshly read account nonce become `retired`, not `included`: another op may have consumed the nonce. Completed, rejected or retired rows can leave the 64-row history; released rows remain until their nonce is used or inclusion is observed. Retired rows still block if the account nonce moves backwards. Old tabs that do not recognize the new journal states fail closed and must reload.
- **Bundled example**: following D-031, check in Counter WASM, its displayed Rust source copy and a source/WASM hash manifest. `build:playground` reproduces them; web tests compare the copy with the canonical source. The page builds from `web/` and `sdk/ts/` alone, as the signet JavaScript image stage does. The VM test intentionally reads the shipped WASM; moving that asset must update this cross-directory test dependency. Browser preflight compiles asynchronously without instantiating it; the node remains authoritative for protocol validation.
- **Presentation and limits**: retain five mobile bottom tabs, with other playground entry points. Node acceptance, execution, a query and BTC finality remain distinct. Polling stops on the node's BTC-final report or the existing attempt/error limits; a new visit or manual refresh checks again, rather than treating a saved report as irrevocable. Recovery cannot cancel copies held by other nodes or coordinate writes from other applications. Clearing site data loses the journal's protection.

## D-119 The contract playground is a page of its own at playground.ksana.network, with seven example contracts that each have a page, and a compile service beside the signet (2026-10-19, asked by the project owner after D-118; moves D-118's page out of `web/` and keeps its journal; revises CLAUDE.md's "Language" and `docs/SIGNET.md`; new example contracts, a new directory with its own dependencies, deployment; no consensus change, M40)

- **Problem**: D-118 put a contract playground inside the web wallet and explorer, `#/playground`: one bundled counter, uploaded WASM, arguments as raw Borsh hex. The
  project owner asked for more examples with an interface each; for editing, compiling and deploying contracts online; for complete contracts and interfaces for the applications a
  chain needs, such as an AMM and lending; for the playground at `playground.ksana.network`; and for a link to it from the home page.
- **Decision**:
  1. **A page of its own**: `playground/`, a React and Vite app like `web/` that builds and deploys apart from it, on the TypeScript SDK (`file:../sdk/ts`). It works with a devnet or
     signet node (the hosted copy with the public signet's node1), signs path B operations in Unisat, OneKey or Xverse (a test key only on a local regtest devnet, as the web page),
     pays the fees in KSA and asks the signet's faucet for test KSA. `web/` no longer has `#/playground`: its top navigation (in a window at least 1200 px wide, below which the top
     bar has no room for it in every language), its footer, the wallet's home and, with the node data source on the signet, a contract's page link the playground in a new tab, a
     contract's page at `#/open?c=<contract ID>`, which opens an example's deployment on its page and any other contract with forms from a pasted source or ABI block. The project's
     site links it from its top bar, its landing page's Build section and its footer, and describes it on `docs/playground`. A page in Simplified Chinese asks for Chinese with
     `?lang=zh-CN`, as for the signet's web page (D-115), and the site's check enforces it for both.
  2. **Seven examples** in `contracts/examples/`, each with a page made for it (the App tab), its source (Code), a form for every method (Methods) and the operations sent from the
     browser (Activity): `playground-counter` (D-118's counter, which now also adds up to 1 000 at a time and emits `changed` with the caller), `mintable-token` (a built-in asset
     whose owner mints up to an optional cap; any holder burns), `nft` (tokens with URIs, approvals and operators, fixed-price listings paid in any asset), `amm` (the existing example,
     unchanged but for an ABI block in its documentation), `lending` (one market: shares for suppliers, borrowing against collateral at a price an admin sets, interest on a utilization
     curve with a kink, liquidation with a bonus, reserves), `multisig` (owners and a threshold; proposals to transfer, to call another contract or to change the owners or the
     threshold) and `vesting` (schedules with a cliff and a linear release, which their creator may revoke). Each has native tests against the SDK's mock host. None needs a new host
     function or any change to the VM (had one needed it, the work would have stopped for the project owner's decision).
  3. **The ABI block**: a contract describes its methods to the playground in a fenced `abi` block of its `//!` documentation: `init`, `call` and `view` methods, `payable`, `event`,
     `struct` and `enum`, with the integers, `bool`, `String`, `Bytes`, `[u8; N]`, the 32-byte IDs, `Option`, `Vec` and tuples, all Borsh as always. The playground parses it for its
     forms and to decode results and events. It is documentation only: nothing on chain reads it, and the node still serves no metadata of a contract.
  4. **Reproducible modules**: `playground/scripts/build-examples.mjs` builds the examples as the compile service does (point 5), in a slot of its own, with the pinned
     toolchain, the contracts workspace's linker flags and the slot's and Cargo's paths remapped, and writes the modules with `builds.json` (the SHA-256 of each source and
     module); the playground's tests fail while a source differs from the one its module was built from. D-031 found contract WASM not reproducible across checkout paths:
     cargo hashes a path dependency outside the workspace's root, the SDK, by its absolute path, and the symbol names, and with them the order of the functions in a module,
     follow that hash. A slot's manifest names the SDK through `contracts/sdk`, a link to `../crates/ksana-contract-sdk` that the slot's workspace excludes, so the hash is
     relative. The machine's architecture enters the same hash: cargo hashes `rustc -vV`, its host line included, into the metadata of build scripts and proc macros (borsh's
     build script, borsh-derive), and through them into the crates for wasm32, so amm and multisig came out otherwise on aarch64 Linux, in the image the project owner's
     rehearsal builds on Apple Silicon (amm `f1729663…` for `518c5d29…`; here, changing nothing but the host line to `aarch64-unknown-linux-gnu` gives the same module). Every
     build runs rustc through `playground/compiler/lib/rustc.sh`, which tells cargo the host `x86_64-unknown-linux-gnu` on every machine and gives a build script's probe for
     that host (proc-macro2's) the machine's own; what runs on the machine is still built for it, as cargo passes no `--target` for build scripts and proc macros. The seven
     modules come out the same in slots at three paths, with a rustc that answers as an aarch64 or a riscv64 machine without an x86_64 standard library, and in the compile
     service's image, and the service's builds of the examples' own sources equal them byte for byte: its tests build every example in a slot of their own and compare, amm and
     multisig again on such a machine, and its image compares every module with `builds.json` when it warms its slots (built on the project owner's Apple Silicon Mac, it gives
     all seven). The checkout keeps its layout (through such a link there, `cargo fmt --all` takes the SDK for a package of no workspace), so builds in it,
     `ksana-test-contracts`' among them, still follow its path, which their tests do not mind; the determinism test keeps its fixed bytecode.
     `crates/ksana-vm/tests/playground.rs` runs every shipped module through the deployment checks of the real VM, and the counter and the token through calls.
  5. **The compile service**, `playground/compiler`: Node.js with its standard library only. It builds one source as one of the example crates
     (`cargo rustc --locked --offline --release --target wasm32-unknown-unknown -p <crate> --crate-type cdylib`) in a copy of the contracts workspace (a slot) with the flags of point
     4, puts the example's source back after each build, and answers the module with rustc's diagnostics, their paths shown as `src/lib.rs`. Limits: 128 KiB of source, 120 s, a
     module of at most 1 MiB, 6 builds a minute per address, one slot and a queue of 8 by default. A build sees a minimal environment (no variable of the service) and runs in a
     process group of its own, killed at the time limit or when the request goes away.
  6. **Hosting**: the page is a Cloudflare Worker with static files only at `playground.ksana.network` (`playground/wrangler.jsonc`), as the
     project's site is (D-112, D-113). Workers Builds runs `npm run build:hosted` (the TypeScript SDK, then the page with `hosted/deployment.json`,
     the signet's endpoints as absolute URLs, and `hosted/_headers`: no frames, no sniffing, no referrer, the hashed assets cached for good) and
     `npx wrangler deploy` at every push to `main` that changes the page, the SDK or the examples. The project owner chose this on 2026-10-19
     over a container on the signet's server that served the page behind the signet's Caddy, at a domain whose record pointed to the server.
     The compile service stays on that server, in the Compose project's `playground` profile, which `signet/server.sh` turns on once the page has
     an origin (`KSANA_PLAYGROUND_ORIGIN`, `setup --playground <name>`): a `compiler` image (the pinned Rust toolchain, the crates fetched when the
     image is built, slots warmed by a build of every example) behind Caddy at `https://signet.ksana.network/compile`. The page calls node1's
     `/rpc`, which allows any origin as before, the faucet's `/faucet` and `/compile` across origins; the faucet and the compile service allow
     the page's origin alone (`KSANA_FAUCET_CORS`, `KSANA_COMPILER_CORS`), so that no other site has its visitors' browsers spend their
     addresses' limits there. The compiler's container is only on an internal network that Caddy alone reaches, with no capabilities,
     `no-new-privileges` and limits on processes, memory and CPU: one core, enough for a build's one rustc, at a low CPU weight, since each
     seed node mines on a core of its own. `signet/deploy.sh <host> playground` builds and recreates the compiler's container and checks its
     health and the page's preflight; `signet/rehearse.sh` checks the compile service, a build of the counter's own source to the shipped module
     included, and which origins node1, the faucet and the compile service allow.
  7. **D-118's journal moves with the playground** (`playground/src/chain/ops.ts`): the record per account before broadcast, one Web Lock per chain and account, a nonce held while
     an outcome is unknown, and explicit local release after `NOT_FOUND`. `web/` keeps D-118's fix that sends `ksana_submitOp` past the batching transport, which covers its Send page.
  8. **Languages**: English (the source language) and Simplified Chinese; the playground opens in English unless `?lang=` or a choice in its settings says otherwise (D-115).
     `playground/src/i18n/zh-CN/` joins the places of CLAUDE.md's "Language" that hold Chinese.
  9. **Dependencies** outside SPEC §12's table, in `playground/` only: CodeMirror 6 (`@codemirror/*` and `@lezer/highlight`, MIT), the editor with Rust highlighting, search and the
     compiler's diagnostics on their lines, which loads nothing from the network; `@types/node`, for the tests and the build configuration; and `wrangler`, the deploy
     tool at the site's version (D-113), a development dependency only. The compile service adds none.
- **Rationale**: a page of its own keeps the explorer and the wallet as they were, builds and deploys apart, and can later leave the repository with the other services without
  touching the web page. The examples are the applications a chain is judged by (a token, collectibles, an exchange, lending, shared custody, vesting), and each shows another part of
  the VM: built-in assets, attached funds, calls to other contracts, the block's time. The ABI block keeps the forms in the same file as the code they describe. The compile service is
  narrow rather than general: only the example crates with their own manifests, so no `build.rs`, procedural macro or dependency of the visitor's runs; offline; in a container that
  holds no secret, since a source can read any file the service's user can (`include_bytes!`) and the variables of its build (`env!`).
- **Costs and limitations**: the service runs rustc on untrusted sources: a compiler bug, const evaluation or macro expansion runs on the server, contained by the container and its
  limits rather than prevented. Edits beyond `src/lib.rs` (another crate, a new dependency) need a local build and an upload. A contract deployed from edited code may not match its
  example's page, which says so. The pages read only what the contracts' views and the node's listings give, so what no view lists (an account's NFT operators, the number of
  listings) is not shown. The examples are for learning and not audited. Settings and the journal live in one browser. The page and the compile service deploy apart: the page at a
  push to `main`, the compile service with the server's deploy (`signet/deploy.sh <host> playground`), and a preview of another branch reaches the node but not the faucet or
  the compile service. The Docker images and the rehearsal run on the project owner's machine (CI's quota is spent), and the byte identity of builds on aarch64 is checked there.
- **Impact**: `playground/` (the page, `scripts/build-examples.mjs`, `compiler/`, `wrangler.jsonc`, `hosted/`, README files); `contracts/examples/` (`playground-counter`,
  `mintable-token`, `nft`, `lending`, `multisig`, `vesting`, and `amm`'s documentation) and `contracts/Cargo.lock`; `crates/ksana-vm/tests/playground.rs`; `web/` (`#/playground`
  and its messages removed, the links, `deployment.ts`, the messages of six languages, `README.md`); `signet/` (`Dockerfile`, `docker-compose.yml`, `Caddyfile`, `services/` (the
  faucet's allowed origin), `server.sh`, `deploy.sh`, `rehearse.sh`, `.env.example`, `README.md`); `site/` (`site.config.mjs`, the top bar, the landing page, the footer, the check,
  the docs pages `playground`, `wallet`, `contracts` and `example-contracts` in both languages); `docs/SIGNET.md`; `.github/workflows/playground.yml`; CLAUDE.md; PROGRESS M40.

## D-120 Token standards KS-20 and KS-721, with their reference implementation in the contract SDK (2026-10-20, asked by the project owner, who chose the names, 32-byte token IDs and approvals with an optional expiry; `docs/STANDARDS.md`; revises SPEC §9.5; the contract SDK, the example token, two test contracts, then the TS SDK and shared vectors, then the node's index of the standards' events with its RPC and the CLI; no consensus change, M41)

- **Problem**: the project owner asked to support both kinds of token, built-in assets and tokens kept by contracts, to give tokens the most freedom, to set a standard for
  fungible tokens and one for NFTs, and to present both kinds as assets. Contracts shared no interface: the example token and the NFT example had methods and events of their
  own, the web wallet guessed what a contract paid from the examples' event layouts (`contractMoves` in `web/src/wallet/model.ts`), a token could reach a contract only through
  `approve` and a second op, and nothing said which contracts were tokens.
- **Decision**:
  1. **Two standards** in `docs/STANDARDS.md`: KS-20 for fungible tokens and KS-721 for NFTs, named after ERC-20 and ERC-721 so that developers know what they are (a search on
     2026-10-20 found no other chain's standard of these names; Kaspa's KRC-20 was the closest). They fix the interface, the events, the invariants and the revert reasons, and
     leave the policy to the token: fees, freezes, allowlists, pauses and limits are the token's own rules, within the standards' rules. They are conventions between contracts,
     wallets and indexers; the VM and block validity know nothing of them.
  2. **Self-declaration**: `init` emits `standards` and then `metadata` before any `transfer`, and the view `standards()` gives the same list for the contract's whole life, the
     standard's ID first and then its extensions'. A module exports only `ksana_call`, so its exports say nothing of its methods, and a registry would need keeping; the
     declaration lets a node index tokens from receipts alone. A declaration is a claim: trust comes from the contract's ID and code hash.
  3. **One step**: `transfer_and_call(to, amount or token_id, data)` moves the tokens, then calls the recipient's `on_token_received` / `on_nft_received` with the funds attached
     to the call passed on unchanged; the recipient answers `Bytes`, which the token returns byte for byte (it checks that the answer decodes as `Bytes`, else
     `malformed return value`). A refusal fails the whole op. The recipient checks that its caller is a token it accepts.
  4. **Approvals with an optional expiry**: `approve(spender, amount, expires_at)` and KS-721's `set_operator(operator, approved, expires_at)` count while
     `block_timestamp() < expires_at`; `allowance` and `is_operator` answer for now, `allowance_info` and `operator_info` as set; `u128::MAX` is an unlimited allowance. The project
     owner chose an optional expiry over a required one and over none; wallets approve exact amounts with an expiry by default.
  5. **Fees come out of the amount** (KS-20 rule 4): the sender's balance drops by exactly `amount`, the recipient gets `amount − fee`, and two `transfer` events say so. The draft
     had the sender pay the fee on top; that loses a pool's or a market's money without an error, as its reserves go down by `amount` while its balance goes down by more.
     Built-in assets charge no fee: the issuer capabilities the project owner chose for them are `mint`, `freeze`, `allowlist` and `pause`.
  6. **Refusals with fixed reasons**: zero amounts, the zero account and the contract itself as recipients, funds attached to any method but `transfer_and_call`; English phrases
     that wallets translate and contracts compare, and `rejected: <reason>` for a token's own rules.
  7. **KS-721 token IDs are 32 bytes** (the project owner's choice over `u64`), serial numbers big-endian in the last 16, so that an ID can hold a hash; every change of owner is a
     `transfer` event and clears the token's approval; `transfer` and `transfer_and_call` are the owner's alone, `transfer_from` checks `from`; burned IDs are never minted again.
  8. **Capabilities**: the view `capabilities()` reports `mint`, `freeze`, `allowlist`, `pause`, `clawback`, `fee` and `upgrade` in that order, while they hold; the first four are
     the names, in the same order, of the issuer capabilities planned for built-in assets, which the project owner chose without `clawback`. Names are 1 to 64 bytes and
     symbols 1 to 16, as planned for built-in assets' names and symbols.
  9. **The reference implementation is a library of the contract SDK**: the modules `standard`, `ks20` and `ks721` (and `conformance`, natively only), not the crates
     `contracts/standards/ks20` and `ks721` the draft named. The playground's compile slots already copy the SDK, a contract built on it needs no new dependency, and a module
     links only what it calls. The modules are behind the SDK's `standards` feature, off by default: declared, they change the module of a contract that does not use them
     (with the feature on, all seven of the playground's shipped examples compile to other bytes, though none of them calls the new code), and with the feature off those
     modules and `builds.json` stay as they are. A contract built on the standards depends on the SDK with `features = ["standards"]`: here the example token and the two test
     contracts, and the playground's examples when its work moves them onto the standards. A token is `Ks20::new()` with `.burnable()` / `.mintable()` and `.with_policy(rules)` (a `Policy` trait: a check before every movement, the fee,
     the capabilities); `Ks20::dispatch` answers the standard's methods and `None` for others. `Ks721` is the same with `.burnable()` / `.enumerable()`. The state lives under the
     storage prefixes `KS20:` and `KS721:`.
  10. **The conformance kit** `conformance::ks20` / `ks721` runs the standards' rules on the mock host against any contract a setup deploys, including the extensions it declares.
  11. **The example token is the reference KS-20 token** (`KS-20-burnable` and `KS-20-mintable`, with an optional minter and cap), and keeps what callers written before KS-20
     send: an `init` of four arguments (no minter, no cap) and `approve(spender, amount)` (no expiry), with the `transfer` event's layout unchanged, so the AMM, the site's
     tutorial, the token-swap example and the regtest tests keep working. The NFT example is rewritten on `ks721`, and the AMM takes tokens by `on_token_received`, in the
     playground's work (D-119): its pages, modules and `builds.json` change with them, so both stay as they are here.
  12. **Left out of v1**: `KS-721-mintable` (collections number and mint tokens in their own ways, so minting is the collection's own method and it reports `mint`),
     `transfer_from_and_call`, `approve_and_call`, signed permits (wallets sign BIP322 messages, which a contract would have to rebuild), royalties, collection URIs.
  13. **Test contracts** `contracts/testing/receiver` (takes both kinds of token by `transfer_and_call` and records what it saw) and `contracts/testing/collection` (the `ks721`
     module's example collection) run the standards in the real VM (`crates/ksana-execution/tests/contracts.rs`).
  14. **Shared vectors and the TS SDK** (the second change): `tests/vectors/standards.json`, written with the contract SDK's own types and encoders
     (`cargo run -p ksana-contract-sdk --example gen_standard_vectors`, kept in step by `tests/standard_vectors.rs`), holds every method's arguments, the views' answers, the
     events and bytes that are no event, token IDs and `AssetRef`; the TS SDK's `standards` module builds the operations, reads the views and decodes the events, checked
     against it and against `docs/STANDARDS.md`'s tables of reasons and capabilities. The encodings are conventions, not consensus, and fixed once contracts use them: a change
     to a standard regenerates the file and is recorded here. The builders refuse what every token refuses (a zero amount, the zero account or the contract itself as
     recipient), which on chain would only cost the op's fee; the views are read one after another, as a node runs at most four read-only calls at a time; and bytes that are not
     exactly one of the standard's events decode to `null`, so that a contract's other events never pass for a transfer.
  15. **The node's index of the standards' events** (the third change; the node's own, beside D-111's operation index in its block store, not consensus): the column family
     `standards` keeps the declared contracts (the declaration and its position, the metadata, the supply, the number of holders, `irregular`), each account's balance of a token
     or number of a collection's NFTs, the holders by amount, each NFT's owner and each account's NFTs, and the approvals; `standards_undo` keeps by height the value each key had
     before that height changed it, so that rolling back the main chain restores the index exactly. Both are written with the operation index in the batches of `connect_main` /
     `rewind_main`, under one marker, `meta` `index`, which replaces D-111's `op_index`: a data directory with the old marker, or none, gets both built again from its stored
     receipts at startup (logged). A contract's first `standards` event is its declaration, and from there on the index applies the events of the standard it names first, each
     only in exactly the standard's layout (a declaration naming both standards is followed as neither, and one outside 1 to 16 IDs of 1 to 64 bytes is kept as none). An event of
     the standard the index cannot apply as the standard has it (a transfer of more than the sender holds by the token's own events, of an NFT `from` does not own, a mint of an
     ID that existed, a second declaration, metadata out of bounds or with other decimals) is passed over and marks the token `irregular`. The accounts the applied events name
     (the zero account left out) and the contract that emitted them join the op's accounts in the operation index, so `ksana_getAccountOps` lists tokens received under their
     recipient and a token's history under the token; a deployment stays off the listing of the contract it deploys, as D-111 has it, so that a page looking for a contract's
     deployment below the earliest op listed under the contract (the web page's explorer, `searchDeployment`) still finds it. Six methods read the index (`docs/RPC.md`):
     `ksana_getContract` (code hash, deployer and declaration of any contract), `ksana_getAccountAssets` (one list of what an account holds: its built-in assets by asset ID, read
     from the state as `ksana_getAccount` reads them, then its tokens and collections by contract ID), `ksana_getToken`, `ksana_getTokenHolders` (the largest first),
     `ksana_getNfts` and `ksana_getApprovals`. They differ from the design draft's §7.1 in four points: a page continues after a cursor, the hex of its last item's key (33, 48,
     64 or 65 bytes), instead of before a position, as these listings are in key order rather than the main chain's; `ksana_getApprovals` is one paged list tagged by kind
     (`allowance`, `operator`, and `token` for the account approved to move one NFT, which the draft left out) instead of two lists without pages, as an account can give any
     number of approvals; `ksana_getContract` also gives the position of the declaration, and `ksana_getToken` the `irregular` flag. The index answers what an account holds, and
     the contracts' views how much: allowances are listed as set, since a `transfer_from` emits no `approve` event, and approvals stay listed once they expire. A node without the
     index answers `-32601`, as D-111's listings do. `ksana-cli` adds `contract`, `token`, `holders`, `nfts` and `approvals`, and `account` the tokens and NFTs from every page of
     `ksana_getAccountAssets`; the TS SDK's `KsanaRpcClient` adds the six methods, with strict parsers.
- **Rationale**: interfaces close to ERC-20 and ERC-721 are the ones developers and auditors know, and the example token's methods already had their names, so the existing
  callers and the AMM keep working. What Ksana's VM allows (calls between contracts, attached built-in assets, catchable calls) makes `transfer_and_call` safe and turns two ops
  into one. The expiry, the zero-amount refusal and the refusal of attached funds answer the losses ERC-20 tokens are known for (forgotten unlimited approvals, poisoned
  histories, tokens stuck in contracts). A library rather than a contract to copy keeps every token on the same audited code, and the kit holds a token that writes its own code
  to the same rules.
- **Costs and limitations**: the reference token's module is 28.7 KB against the example token's 11.4 KB before; deploying it costs 777 624 gas (380 365 before), still within the
  CLI's default `--max-gas` of 1 000 000, and each call about 20 000 more, most of it loading the larger module (a `transfer` 83 369 against 63 300, `approve` 67 596 against
  47 777, `transfer_from` 99 037 against 78 211). Path B fees follow `max_gas`, not the gas used, so the fee of an op with the default `--max-gas` does not change. The standards
  bind only contracts that keep them: a declaration is a claim, and a contract can emit false events about itself. Built-in assets' issuer capabilities came with D-121, and
  receipts that record built-in assets' movements and management actions with D-122; the wallet's single list of assets follows in a later change. The node's index (the third
  change) costs every connected block a read of its successful ops' events and a few writes per standard event, an undo log kept for good, as the operation index is, and a data
  directory from an earlier version one pass over its stored receipts at its first start. `ksana_getAccountAssets` reads the built-in balances as `ksana_getAccount` does, by a scan
  of every balance in the state; an index of them by account is left for later. Pages read at different tips can miss or repeat an item that moved between them.
- **Impact**: `docs/STANDARDS.md`; `crates/ksana-contract-sdk` (`standard.rs`, `ks20.rs`, `ks721.rs`, `conformance.rs`, `lib.rs`, the `standards` feature in its manifest);
  `contracts/examples/token` (rewritten on `ks20`, its tests on the conformance kit); `contracts/testing/receiver` and `contracts/testing/collection` (new) and `contracts/Cargo.lock`; `crates/ksana-test-contracts`
  (`RECEIVER`, `COLLECTION`); `crates/ksana-execution/tests/contracts.rs`; SPEC §9.5; `docs/ABI.md` (a pointer); CLAUDE.md; PROGRESS M41. The second change: the contract
  SDK's example `gen_standard_vectors`, its test `standard_vectors.rs` and dev-dependencies (`serde_json`, and itself with `standards`); `tests/vectors/standards.json` and the
  vectors' README; `sdk/ts` (`src/standards.ts`, the `bool` type of `src/schema.ts`, `test/standards.test.ts`, the README); the site's SDK page in both languages. The third change:
  `crates/ksana-node` (`standards.rs`, new; `store.rs`: the column families `standards` and `standards_undo`, the marker `index`, `rebuild_index` and the index's reads;
  `chain.rs`: the build at startup; `rpc.rs`: the six methods; the tests `m41_standards_index.rs`, new, and `m35_op_index.rs`); `crates/ksana-rpc` (the six methods and their
  JSON types); `crates/ksana-cli` (the commands, `tests/standards.rs`, new); `sdk/ts` (`src/rpc.ts`, `test/rpc.test.ts`, the README); `docs/RPC.md`; `docs/STANDARDS.md`
  (two pointers to it); the site's RPC, CLI and SDK pages in both languages; CLAUDE.md; PROGRESS M41.

## D-121 Built-in assets carry a symbol and may declare issuer capabilities when they are created: mint, freeze, allowlist and pause, which can only be given up; no clawback (2026-10-20, chosen by the project owner; revises D-073's assets and SPEC §2, §8.6, §9.2, §9.3, §9.5, §10.1, §11.2, §13, §14, §16, `docs/ABI.md`, `docs/RPC.md`; consensus change: the encodings of `AssetMetaInput` and `AssetMeta`, a state key, seven host functions and their gas; devnet / signet restart from genesis, M42)

- **Problem**: a built-in asset (D-073) could only be issued, transferred and burned. Its issuing contract could not freeze an account or pause the asset, so an issuer that must
  follow lawful orders (a payment stablecoin's blacklist) could not use a built-in asset, and nothing on chain told a holder whether the supply could still grow. A built-in asset
  also had no symbol, so wallets showed its name where a ticker belongs. The project owner asked how chains that offer only primitives (Sui, Aptos, Solana) handle a USDT-style
  blacklist, a meme coin's transfer fee and further issuance, and then settled on two paths: contract tokens under a token standard for any logic (drafted at the same time; D-120
  and M41 are reserved for it), and built-in assets with the necessary modules, without a consensus change for every new rule. The design compared an issuer hook called on every
  transfer with a fixed set of native capabilities; on 2026-10-20 the project owner chose the capabilities without clawback, and a symbol in the same upgrade.
- **Decision**:
  1. **Capabilities**, a bit set held by an asset's issuing contract: `mint` (`0x01`, it issues more), `freeze` (`0x02`, it freezes accounts), `allowlist` (`0x04`, only the
     accounts it allows hold and move the asset) and `pause` (`0x08`, no account but itself sends the asset). `asset_create` declares them when it creates an asset; the first
     `asset_issue` creates one with `mint` alone, as every asset behaved before. They can only be given up (`asset_renounce`), never gained; bits `0x10` and above are undefined
     and refused, room for a later upgrade. KSA has none.
  2. **No clawback**: the project owner declined the fifth capability of the design, a seizure moving any account's balance to the issuer. SPEC §9.2's invariant stands: an
     account's assets move only by its own signature, a contract's only by the contract. An issuer can stop an account's asset, never take it.
  3. **State**: `AssetMeta` (`0x03`) becomes `{ name, symbol, decimals, total_supply, issuer, capabilities, paused }`, and `0x0C ‖ asset ‖ account → u8` keeps an account's status
     flags for an asset (`0x01` frozen, `0x02` allowed), deleted when no flag is left. The design kept the capabilities in a record of their own, absent at the default; since the
     symbol changes `AssetMeta`'s encoding and restarts the networks anyway, they became two fields of it, read in the lookup of the metadata every movement already makes. The two
     flags are independent bits, so an account can be frozen while allowed.
  4. **Rules** (`ksana-execution` `builtin.rs`): every movement of an asset passes through `transfer`, `issue` or `burn`: the `AssetTransfer` action, the funds attached to a
     `Deploy`, a `Call`, `call_contract`, `try_call_contract` or `create_contract`, `asset_transfer`, `asset_issue`, `asset_create` and `asset_burn`. KSA's fees, unlocks and
     rewards change balances directly, and KSA has no capabilities. The issuing contract is exempt from every rule. With `freeze` a frozen account neither sends, receives nor
     burns the asset; with `allowlist` only the allowed accounts do; while the asset is paused no account but the issuing contract sends or burns it, and receiving from the
     issuing contract goes on. A flag counts only while the asset holds its capability; setting a flag, or pausing, needs the capability, and clearing one, or resuming, never
     does, so giving up `pause` resumes the asset and giving up `allowlist` opens it to everyone. The issuing contract has no status for its own asset. An issuance needs `mint`,
     and its recipient must be able to receive. A refusal fails the op as an insufficient balance does, with a message starting with `rejected:` on every path (`rejected:
     sender is frozen for this asset`, `rejected: recipient is not on this asset's allowlist`, `rejected: asset is paused`, `rejected: asset supply is fixed`), the funds
     attached to a `Call`, a `Deploy` or the RPC's `ksana_call` included, whose other failures read `attached funds transfer failed: …` (the review of D-121, F1);
     `try_call_contract` catches a refusal of its attached funds as status 2. An asset created with an allowlist can only go to its issuing contract until it allows an account.
  5. **Metadata**: a name of 1 to 64 bytes, a symbol of 1 to 16 bytes and at most 18 decimals, the token standard's limits, written by the asset's creation and fixed as before;
     neither name is unique. Consensus checks the lengths in bytes and UTF-8 (which Borsh decoding checks), never Unicode character classes, whose tables change with the Rust
     version. `MAX_ASSET_META_BYTES` stays 1 024. KSA's symbol is `KSA` in the genesis state.
  6. **Host functions** (31 in all, `docs/ABI.md` §3): `asset_create(sub_id, meta, meta_len, capabilities, to, amount, dst)` at 20 000 + 1/B of meta, as `asset_issue`;
     `asset_freeze(sub_id, account, frozen)`, `asset_allow(sub_id, account, allowed)`, `asset_pause(sub_id, paused)` and `asset_renounce(sub_id, capabilities)` at 10 000, as
     `asset_transfer`; `asset_policy(asset) -> i64` (the capabilities, `0x100` while paused, `-1` for no asset) and `asset_status(asset, account) -> i32` at 2 000, as
     `asset_balance`. The management functions name the asset by the calling contract and a `sub_id`, so only its issuing contract manages it; the two queries serve any
     contract. A switch other than 0 or 1, or a capability set outside `u8`, traps in the VM. The design's one `asset_set_status(…, status)` became `asset_freeze` and
     `asset_allow`, one per flag and capability. The VM charges a function's gas before the host acts.
  7. **RPC and clients**: `ksana_getAsset` adds `symbol`, `capabilities` (names in bit order) and `paused`; `ksana_getAssetStatus(asset, account)` answers an account's flags and
     whether it can send and receive the asset now, through the same `builtin::can_send` / `can_receive` as the rules, so a wallet can check before paying for a refused op. The
     TS SDK (`getAsset`, `getAssetStatus`), the CLI (`asset-status`) and the web wallet (an asset's recorded symbol as its unit, with the lookalike guard of `docs/WEB_DESIGN.md`)
     follow. The examples take a symbol: `asset-token` creates its asset with `asset_create` and no capability, a fixed supply the protocol keeps; `mintable-token` (the
     playground's module and `builds.json` rebuilt, the other six unchanged), `btc-payment-trigger` and the `token-swap` SDK example (`--symbol`); the test contract `probe`
     calls the new functions (`tests/data/probe.wasm` replaced) and the determinism scenario's block 14 uses every capability.
- **Rationale**: the necessary modules of an asset are the compliance floor, and native assets elsewhere converged on the same set: Algorand's freeze and default-frozen
  assets, Stellar's authorization flags, Solana's freeze authority and Token-2022's default account state, Aptos's references made only at creation, Sui's deny list. Rules
  beyond it (a transfer tax, limits, royalties) belong to contract tokens. Checks in `builtin.rs` run no contract code inside a transfer: no reentrancy, exact gas, and a bug in an
  issuing contract cannot stop its asset's transfers. Declared at creation and only given up, the capabilities are visible before anyone holds the asset. The symbol is cheapest
  now: the asset module and the networks' state restart anyway, and after mainnet it would need a migration of every asset's record.
- **Alternatives**: an issuer hook called on every transfer (it runs the issuer's code inside every transfer, the two paths outside the VM included; 40 000 to 62 000 gas a
  transfer as measured on the design's prototype, against 10 000; it lets an issuer veto a pool's or a lending market's payouts by rules nobody can check in advance, and a bug in
  it stops every transfer); clawback (declined, above; it would also have let an issuer drain a pool or a lending market below the reserves it records); a transfer fee on assets
  (paid by the sender, it makes pools pay out more than they record; deducted from the amount received, it breaks the checks of attached amounts; a contract token can charge
  one); a symbol from an optional view of the issuing contract (optional, unchecked, a call per asset).
- **Costs and limitations**: holders of an asset with capabilities can be stopped by its issuing contract, and so can a pool or a lending market holding it; the example AMM and
  lending market do not check `asset_policy` yet, and the web wallet shows the symbol but not yet the capabilities or the standing of an account, so only `ksana_getAsset`
  tells that an asset can be stopped. Further consequences (the review of D-121, S2): the issuing contract trades while its holders cannot, as a pause never stops it and it
  has no status to freeze, so during a pause it can sell into a pool at the pool's price while no one else can trade, and it can freeze chosen holders before it sells; a
  freeze reaches the asset paired with this one, as the example AMM's `remove_liquidity` pays out both sides in one op, so freezing a liquidity provider, freezing the pool or
  taking it off the allowlist, or pausing the asset, fails the whole withdrawal and the other side stays in the pool with it (a one-sided exit would bound this, and the AMM's
  and the lending market's checks should treat `pause` and `allowlist` like `freeze`); an issuing contract that is a proxy, or calls `delegate_call`, uses its capabilities
  with whatever code it delegates to, as the management functions name the current frame's contract, which `delegate_call` keeps; symbols are not unique, and only the web
  wallet's lookalike guard looks at them. A movement of an asset with `freeze` or `allowlist` reads the two parties' status keys at the flat price of a transfer, 10 000 gas (the
  review's S1). By the storage prices the two reads come to about 4 100 gas beside the about 28 000 of a transfer's own reads and writes, some 15%. `examples/transfer_reads.rs`
  measures them over 10 000 accounts in RocksDB on a 4-core x86_64 machine: they add about 23 µs a transfer, two thirds more execution (34 to 56 µs) and some 14% of a plain
  transfer's execution and commit together (161 µs), close to that share. The totals it prints, 161 and 171 µs, differ by less, as the two commits, which write as many balances and
  should cost the same, came out at 127 and 115 µs. A block's body, at most 80 KiB, holds fewer than 500 transfers, so the reads add at most about 11 ms to the execution of a
  block's own entries; the price stays flat until D-013's calibration prices transfers by measurement. A contract can combine `asset_policy` and `asset_status` into an account's
  standing for every account but the issuing contract, which no host function names; a caller can prove it by the `sub_id` that makes `sha256(issuer ‖ sub_id)` the asset's ID.
  Receipts did not list the management actions until D-122. A flag set before its capability is given up stays in the state, counting for
  nothing: clearing them all would need a scan that gas cannot price. Every state root changes with KSA's metadata in the genesis state, so devnet and the public signet restart
  from genesis; the signet's restart clears everyone's balances there and waits for the project owner's word. After mainnet a change like this one is a protocol version (SPEC
  §6.9), with the import allowlist branching on the version.
- **Impact**: `ksana-primitives` (`asset.rs`: limits, capability and status bits and names, `check_meta`); `ksana-execution` (`keys.rs` `AssetMeta` and the `0x0C` key,
  `builtin.rs`, `host.rs`, `genesis.rs`, `executor.rs`; tests `asset_rules.rs`, `asset_capabilities.rs`, `asset_properties.rs`, `asset_token.rs`, `rules.rs`, the determinism
  scenario and its golden roots, `tests/data/probe.wasm`; the example `transfer_reads.rs`); `ksana-vm` (`engine.rs`, `host.rs`, `gas.rs` and its table test, `validate.rs`;
  tests `asset_capabilities.rs`, `exec.rs`, `playground.rs`); `ksana-contract-sdk` (`asset_create` and the management wrappers, the mock host); `ksana-rpc`, `ksana-node`
  (`rpc.rs`, test `asset_rpc.rs`), `ksana-cli`; `ksana-indexer` (test `asset_capabilities.rs`); `devnet/smoke.sh` (an asset with the freeze capability); `contracts/examples` (`asset-token`,
  `mintable-token`, `btc-payment-trigger`) and `contracts/testing/probe`; `fuzz/` (the seven functions in `host_params`, seeds); `sdk/ts` (`rpc.ts`, the `token-swap` example);
  `web/` (the asset's unit, `docs/WEB_DESIGN.md`); `playground/` (the token page's symbol, the rebuilt module); `site/` (host functions, RPC, SDK, accounts, parameters and
  example contracts in both languages); SPEC, ABI, RPC, CLAUDE.md, PROGRESS M42.

## D-122 Receipts record the changes an op made to built-in assets: their movements and D-121's management actions (2026-10-20, a part of the project owner's request of D-120 to present both kinds of token as assets; revises `docs/RPC.md`; the node's receipts, its block store's format, the operation listings, the RPC and the TS SDK; no consensus change, M41)

- **Problem**: a built-in asset's movements and its issuer's actions left no record an RPC client could read. The `AssetTransfer` action names its recipient, but a contract's
  `asset_transfer` (a pool's payout), the funds a contract attaches to the calls it makes and the contracts it creates, issuances, burns and D-121's freezes, allowlist changes,
  pauses and renouncements showed only as balances and state that had changed. A wallet could not show a payout or an issuance it received, nor an explorer an asset's history,
  while the standards' events give contract tokens both (D-120 item 15); the project owner asked for both kinds of token to be presented as assets, and D-121 left the receipts of
  its management actions to this change.
- **Decision**:
  1. **The receipt's changes**: `Receipt.asset_changes` lists the changes an op made to built-in assets in the order it made them, each an `AssetChange { events, asset, kind }`
     whose `events` is the number of the op's events recorded before it, so that the receipt's two lists interleave as the op made them (the reference token's
     `transfer_and_call` emits its `transfer` event and then passes the attached funds on, a change with one event before it). The kinds: `Transfer { from, to, amount }` (the
     `AssetTransfer` action), `Funds { from, to, amount }` (attached by the signer to a `Call` or a `Deploy`, by the `caller` to the RPC's `ksana_call`, or by a contract to
     `call_contract`, `try_call_contract` or `create_contract`), `Payout { from, to, amount }` (`asset_transfer`), `Issue { to, amount }` (`asset_issue`, `asset_create`),
     `Burn { from, amount }` (`asset_burn`), `Create { capabilities }` (`asset_create`, or the first `asset_issue` with `mint` alone; the issuance follows as an `Issue`
     unless it is of 0), `Freeze { account, frozen }`, `Allow { account, allowed }`, `Pause { paused }` and `Renounce { capabilities }` (those of the call the asset still had:
     what it gave up, none when it had none of them).
  2. **What is recorded**: every call on the paths of D-121 item 4 that succeeded, recorded by the execution layer's host (`OpHost`) and executor once the builtin returned,
     never by `builtin.rs`, whose rules stay as they are. A change goes into the op's log beside its events, so a level that `try_call_contract` reverts drops its changes with its
     events, and a failed op has none. KSA's fees, unlocks and rewards are not changes (they move KSA outside those paths), nor is a movement of 0, which changes nothing and
     which KS-20 refuses so that histories carry none (D-120 item 6). A movement to the account it comes from is recorded, as KS-20 emits a `transfer` for one, and a
     management call is recorded as made, also when it sets what was set.
  3. **The VM tells attached funds apart**: it moves the funds a contract attaches through `Host::transfer_attached`, a new provided method that defaults to
     `asset_transfer`, so the VM's behavior and gas do not change; the execution layer's host records them as `Funds` rather than `Payout`.
  4. **Not consensus**: receipts enter neither the state root nor the block hash, and gas, the state and every rule stay as they are, so no network restarts. The determinism
     test compares a digest of each block's receipts (`{:?}`) between its processes, so the changes join that comparison; its golden roots stay.
  5. **The node's block store**: a block's `receipts` value starts with `RECEIPTS_MAGIC` (`u32::MAX` little-endian, a number of entries no block reaches) and the format byte 2,
     then Borsh of the receipts. A value without the magic is Borsh of the receipts as a version before D-122 stored them: it reads back with no changes, marked as not
     recording them (`StoredReceipts.asset_changes`); an unknown format byte is a storage error. No pass at startup and no new column family: old values stay as they are.
  6. **RPC**: `asset_changes` in the receipts of `ksana_getOpStatus`, `ksana_getBlock` and `ksana_call`, each change with its `kind` in snake case, `asset` and the accounts in
     hex, amounts as decimal strings, capabilities by name in bit order (as `ksana_getAsset` has them) and `events` as a JSON number (D-007); `null` when not recorded: a
     receipt the node stored before D-122, or any receipt of a node from before. `ReceiptJson` reads a missing field as `null`.
  7. **Operation listings** (D-111): an op that succeeded is also listed under the accounts its changes name (both sides of a movement, the account of a freeze or an allowlist
     change) and under each asset but KSA, so that an asset's own listing is its history; KSA's ID is the zero account's 32 bytes, which no one holds. A deployment stays off the
     listing of the contract it deploys, also when it attaches funds (D-111, D-120 item 15).
  8. **The TS SDK**: `Receipt.asset_changes`, an `AssetChange[]` by `kind` or `null`, parsed strictly (an unknown kind or field is refused); a missing field reads as `null`.
- **Rationale**: one list in the op's order, with the events before each change, gives a wallet and an explorer the whole of what an op did to built-in assets without running
  the VM, as the standards' events do for contract tokens, in a few dozen bytes a change. Recording outside `builtin.rs`, once each call succeeded, keeps the consensus rules and
  gas exactly as they are, and the op's log makes a revert drop changes the way it drops events. The magic prefix reads old values in place, with neither a migration nor a
  restart. KSA's fees and rewards follow from the block and from `ksana_getAccount`'s `locked_fees`, and recording them would add a change to every op and unlock entry.
- **Alternatives**: events the host emits for built-in assets (they would mix with contracts' events under a reserved contract ID, and every client would decode bytes to read
  them); a column family of changes beside the receipts (a second read for every receipt and a second format to keep in step); executing again, at startup, the blocks stored
  before D-122 to fill their changes in (each needs its parent's state and the BTC view of its time, for the blocks between D-121's restart and this version); recording KSA's
  fees, unlocks and rewards (above).
- **Costs and limitations**: clients update in step, as they refuse fields they do not know (D-007, D-078): a CLI or a TS SDK from before refuses receipts with the new field,
  so the public signet's nodes, its web page and its services are deployed together, as the default deploy does, and the contract playground, which Workers Builds rebuilds at
  the merge, reads receipts with and without it. Receipts the signet's nodes stored before they run this version read `null` for good, and an operation index built again from
  them lists their ops by their actions and standard events alone. A change takes 38 to 117 bytes in the block store, and every op that moves an asset has at least one. The
  wallet's and the explorer's use of the changes, with the single list of assets, comes with M41's fourth change.
- **Impact**: `crates/ksana-execution` (`state.rs`: `AssetChange`, `ChangeKind` and the op's log; `host.rs`, `executor.rs`, `lib.rs`; the test `asset_changes.rs`, new, and
  `unlock.rs`); `crates/ksana-vm` (`host.rs`: `transfer_attached`; `engine.rs`); `crates/ksana-node` (`store.rs`: the receipts' format, `StoredReceipts`, the listings; `rpc.rs`;
  the test `m41_asset_changes.rs`, new); `crates/ksana-rpc` (`ReceiptJson.asset_changes`, `AssetChangeJson`); the tests of `crates/ksana-indexer` and `crates/ksana-cli`;
  `sdk/ts` (`src/rpc.ts`, `test/rpc.test.ts`, `test/standards.test.ts`); the receipts of `web/`'s tests and `playground/`'s stand-in node; `docs/RPC.md`; the site's RPC,
  CLI and SDK pages in both languages; CLAUDE.md; PROGRESS M41.

## D-123 The web page shows KS-20 tokens as assets beside the built-in ones, folds away those an account never asked for, and sends a token by its `transfer` once the node has tried it (2026-10-20, a part of the project owner's request of D-120 to present both kinds of token as assets; revises `docs/WEB_DESIGN.md`; web page only, no change to the node, the RPC, the SDK or consensus, M41)

- **Problem**: the node indexes the standards' events (D-120) and its receipts record the changes to built-in assets (D-122), but the web page showed built-in assets
  alone: a wallet holding a KS-20 token saw nothing of it, a token's transfer read as a call with 48 bytes of arguments, a payout or an issuance reached an account without
  its activity saying so, no page described an asset, and the send page sent built-in assets only. The project owner asked for both kinds of token to be presented as assets.
  `docs/STANDARDS.md` §7 asks wallets to show an unverified token's whole ID, to fold away tokens nobody asked for and to suggest `transfer_and_call` when the recipient is a
  contract. A token's transfer costs what its code costs (83 369 gas for the reference token's, D-120), which the 50 000 max_gas the send page gives a built-in transfer does
  not cover, and a transfer the token refuses (STANDARDS §5.1) pays its fee all the same.
- **Decision**:
  1. **A KS-20 token is an asset**: its ID is its contract's, which is also its issuer (`AssetMeta.standard`); its name, symbol and decimals are the index's
     (`ksana_getToken`), cleaned and compared with KSA's and bitcoin's as a built-in asset's are, the symbol cut to 8 characters, and the symbol its ID gives ("Token 0841ac")
     where either reads as KSA or bitcoin, or before the token has emitted its metadata, its decimals 0 until then. The page keeps tokens apart from built-in assets
     (`RpcChain.tokenBook`, `web/src/chain/rpc/tokens.ts`): an `AssetTransfer` naming a token's contract names no built-in asset, and no built-in asset's ID is a contract's,
     being a hash of its issuer and a sub-ID (D-121). The wallet's assets card, an account's balances and what a contract holds list built-in assets and KS-20 tokens as one
     list, by symbol with the look-alikes last, as before; a token's line under its symbol reads "KS-20 · <contract>" where a built-in asset's reads "Issued by <contract>".
     KS-721 collections are listed beside them by the number of NFTs held: the wallet's card "NFTs", a table on an account's page, a row on a contract's.
  2. **Holdings come from the index**: an account's tokens and collections are `ksana_getAccountAssets`, at most 1 000 of them, read when a page first shows the account,
     again once the page has seen an operation name it, and otherwise at most once a minute, as the node adds them up from every balance it indexed. A token's balance in
     the wallet is the index's, and its BTC-final amount takes off the token's transfers, by its events, in the operations not final yet. A node without the index
     (`-32601`) leaves the page with the built-in assets alone, as before.
  3. **What the wallet folds away** (STANDARDS §7): the wallet's list shows KSA, the assets an operation the account signed moved (sent them, attached them to a call or a
     deployment, called or deployed their contract, or had its receipt's changes or a standard's events name them), and those its user kept on this device, per network and
     account. Any other asset is folded under "Other assets (n)" with its whole ID, its issuer and "Keep"; a kept one has "Fold away"; collections fold the same way, and the
     send page lists the folded assets in a group of their own after the others. The explorer's account pages fold nothing: they describe an account, not a wallet. The
     wallet goes by the operations the page has read of the account, its newest and the older ones a reader asks for: an asset that only older operations moved stays
     folded until they are read or it is kept.
  4. **Asset pages**, `#/asset/<id>`: a built-in asset's name, symbol, decimals, supply, issuer, capabilities and pause (`ksana_getAsset`, D-121, read again after a receipt
     records a pause or a renouncement), the connected account's standing in it (`ksana_getAssetStatus`) where its issuer can freeze, allowlist or pause, and its history,
     the node's listing under the asset's ID (D-122), which KSA has none of; a KS-20 token's or a KS-721 collection's standard and extensions, its contract's whole ID, its
     decimals, its supply or number of NFTs, its number of holders, the block of its declaration, its deployer, a warning where the index passed over an event of it
     (`irregular`), its 10 largest holders (`ksana_getTokenHolders`) and its history, the listing under the contract, below a note that anyone can deploy a token under any
     name. A token's figures are read again with its largest holders, at most once a minute while its page shows, so that the number of its holders counts the list. Every
     symbol in the lists links to its asset's page; the contract page names the standard a contract declared and links to its asset's page; the search finds an asset by
     its symbol, its name or the start of its ID, and offers a whole ID it does not know as an asset's too.
  5. **An operation's asset changes and a standard's events**: the operation page lists the receipt's changes (D-122) in the op's order, each with its kind, its asset, the
     accounts it names and its amount in the asset's units, and says where the node did not record them; a standard's events of a contract that declared that standard,
     its declaration read from `ksana_getContract`, are read by the standard's layout (the TS SDK's `decodeStandardEvent`) field by field, amounts in the token's units; any
     other event as before. A known token's `transfer(to, amount)` reads as a transfer of the token, in the lists and on the page ("Transfer 12.5 TKN").
  6. **What reached an account**: in someone else's operation, the payouts, issuances and attached funds its receipt records for the account and a known token's transfers
     to it, an activity row reading "Received" with the amount where a single asset reached it (an operation calling the account itself keeps its own verb). The BTC-final
     balance takes the recorded changes and the tokens' transfers as exact, and falls back to the operation's action and the example contracts' events, with "≤", only for
     a receipt that did not record its changes.
  7. **Sending a KS-20 token**: the send page offers the account's tokens beside its built-in assets. A token's transfer is a call of its `transfer(to, amount)` with no
     funds (STANDARDS §3.1, the TS SDK's `ks20Transfer`), on either path, and a path B one that waited too long switches to path C with its recipient, token and amount.
     The balance it is checked against is the token's own answer (`balance_of` through `ksana_call`), the index's until the token answers, read again as the account's
     operations change. Before the review the page tries the transfer with `ksana_call` as the sender (`caller`), 400 ms after the form stops changing and again as the
     account's operations change, and asks again up to 3 times, 1.5 s apart, while the node is busy (`-32009`):
     - max_gas defaults to the gas the try used plus half, rounded up to 10 000, at least 50 000, and to 200 000 where the try could not run, which the review then warns
       of; a max_gas typed in is kept, as before;
     - a refusal holds the review back and is said under its button: the standards' fixed reasons in the page's language (STANDARDS §5.1), a `rejected: ` reason as the
       token's own rules forbidding it, any other in the contract's own words, cut at 160 characters;
     - the token's own contract is refused as recipient before anything is tried, as the SDK refuses to build that transfer; a recipient the page knows as a contract
       (one it saw act as one, or one `ksana_getContract` answers for, asked for every recipient) is warned of in the review: a plain transfer does not notify it, so a
       contract that does not take KS-20 can neither use nor return the tokens, and `transfer_and_call` is the way to hand them to one; for a built-in asset, that funds
       attached to a call of the contract are the way to pay it;
     - the review gives the whole ID of any asset but KSA, and the op's raw fields read the call.
  8. **Nothing new on the node**: the page reads D-120's index, D-122's receipts and `ksana_call`, all there already; the simulator has no tokens, and gains only the asset
     pages and the search by asset.
- **Rationale**: a user holds tokens, whichever way each was issued, and the project owner asked for one presentation of both. Keeping tokens apart from built-in assets
  in the page's data keeps an `AssetTransfer` from reading as a token's. The index answers an account's holdings in one read, where `balance_of` would take a read-only
  call per token and account at every refresh, which the node runs 4 at a time; the send page asks the token itself for the one balance it spends from, as the index may
  lag behind the token, or differ from it for an irregular one. Folding follows STANDARDS §7: anyone can send any account a token named anything, and "an operation it
  signed moved it" is a rule a user can predict, with keeping a preference of the device. The try says a refusal that would cost a fee in a sentence before anything is
  signed, and fits max_gas to the token: a token's transfer costs what its code does, against 10 000 for a built-in transfer, and half again leaves room for the state to
  change between the try and the block. The fee does not follow max_gas there: with the networks' limits (`BLOCK_GAS_LIMIT` 100 000 000, `MAX_BLOCK_DATA_BYTES` 80 KiB) a
  max_gas below about 295 000 counts for less than the 242 bytes of a token transfer's op signed by a wallet (`op_cost`), which pays for its bytes, 20 more than a built-in
  transfer's.
- **Alternatives**: tokens listed apart from built-in assets (not the presentation the project owner asked for); an account's token balances by `balance_of` (above);
  every asset an account was sent shown (STANDARDS §7); one max_gas for every token (too little for a costly one, and a refusal said only once its fee is paid);
  `transfer_and_call` for every send (it fails the whole op when the recipient is no contract, D-120, so the page sends `transfer` and warns of contracts); sending NFTs from
  the send page (an NFT is chosen, not counted: left to a later change, the explorer shows them).
- **Costs and limitations**: while a token is chosen, a read-only call each time the form settles or the account's operations change, and the token's `balance_of` as
  often. The try runs on the node's tip, so an operation executed before this one can still make it fail, as any op can. An account's holdings can be a minute old where
  the page has seen no operation of it, and a token's supply, holders and largest holders on its page as old. Kept assets live in one browser. NFTs are shown, not sent,
  and tokens are neither attached as funds nor bought by top-up from the page. The simulator has no tokens, so the token's send is checked against nodes: the stage
  `sendToken` of `web/scripts/e2e-devnet.mjs`, given the reference token's module (`TOKEN_WASM`), deploys it, sends some by path B and checks the recipient's balance with
  the token's `balance_of`.
- **Impact**: `web/src/chain/rpc/tokens.ts` (new: `TokenBook`), `chain.ts`, `convert.ts`; `web/src/chain/types.ts`, `views.ts` (`assetOf`, `transferOf`, `ks20TransferArgs`,
  `receivedBy`, `recordedMoves`, `collectionsOf`, the search), `sim/engine.ts`; `web/src/wallet/assets.ts` (new: what the wallet folds away), `tokenSend.ts` (new),
  `model.ts`; `web/src/pages/Asset.tsx` (new), `wallet/Assets.tsx` (new), `wallet/Wallet.tsx`, `wallet/Send.tsx`, `Account.tsx`, `Contract.tsx`, `OpDetail.tsx`;
  `web/src/components/AssetChanges.tsx` and `Collections.tsx` (new), `Lists.tsx`, `Refs.tsx`, `SearchBox.tsx`; `web/src/app/App.tsx`, `router.ts`; the messages of the six
  languages (`asset.ts`, new); the unit tests (`tokens.test.ts`, `assets.test.ts` and `tokenSend.test.ts`, new; `views.test.ts`, `model.test.ts`, `chain.test.ts`);
  `web/scripts/e2e-devnet.mjs`, `web/README.md`; `docs/WEB_DESIGN.md`; the site's wallet pages in both languages; PROGRESS M41.

## D-124 Contracts have a reference written for AI assistants, held to the code by tests and copied from the playground in one press, and the site has a tutorial, an SDK reference and llms.txt files (2026-10-21, asked by the project owner; the playground, the site, one new example contract and tests; no consensus, VM, node, RPC or SDK change, M43)

- **Problem**: the project owner asked whether contracts need a guide to the methods and functions they can use, or a tutorial, and asked for contract material in the
  playground that an AI assistant can be given in one click. The site explained writing a contract, the host functions with their gas, the example contracts, assets and
  the token standards, but no page listed the contract SDK's functions with their signatures, no tutorial went from nothing to a contract deployed from the browser, and
  nothing was written for an assistant. Asked about a Ksana contract, an assistant knows nothing of the SDK, so it makes up an API in the style of another chain's, with gas
  and limits to match: the code does not compile, or compiles and does something else. Material that drifts from the code teaches the same mistakes, so whatever is
  written must stay true as the SDK, the gas table and the playground change.
- **Decision**:
  1. **One reference for AI assistants**: `playground/src/ai/ksana-contracts.md`, one Markdown file in English, about 70 KiB (some 20 000 tokens), that needs nothing
     else: rules for the assistant (what a contract may use, its fixed skeleton, the encoding, never panicking, the safer choice where a request leaves one
     open, said in a sentence), how a contract runs, a complete contract with its test, setting up, testing and building locally, the playground and its compile
     service, the ABI block, arguments as bytes, deploying and calling with `ksana-cli`, why an operation fails, gas, limits, every public item of the contract SDK as a
     Rust declaration without its body and a line on what it does, built-in assets, KS-20 and KS-721 with the SDK's reference implementation, and the mistakes to avoid.
     English, the language of the code, the identifiers and the revert reasons, which assistants read best; the Code tab asks for answers in the page's language.
  2. **Tests hold it to the code**, so that a change it no longer describes fails until it does:
     - the SDK's declarations, both ways (`crates/ksana-contract-sdk/tests/reference.rs`): the test reads the SDK's sources and requires each module's block of
       declarations in the reference to list every public function and method, constant, type alias, struct with its public fields, enum with its variants and trait with
       its methods, as the source declares it, and nothing else;
     - its complete contracts are repository files shown verbatim, built and run: the tutorial's poll (`contracts/examples/poll`) with one of its tests, and a KS-20 token
       and an asset with freeze (`crates/ksana-contract-sdk/tests/reference/token.rs` and `points.rs`), run natively, the token through `conformance::ks20` and its ABI
       block against its dispatch;
     - its gas tables, its limits table and the networks' parameters are built row by row from the constants of the VM, the primitives and the genesis config
       (`crates/ksana-execution/tests/reference.rs`);
     - what it says of the playground (`playground/src/ai/reference.test.ts`): each example page with its crate and whether the crate has the standards, the compile
       service's limits and the faucet's amount as their code sets them, the ABI block's types as the parser takes them, and every ABI block it shows, parsed by the
       playground's parser.
  3. **The playground copies it**: the page `#/ai` ("AI assistants", in the navigation and on the home page) copies the reference in one press, downloads it as
     `ksana-contracts.md` or opens it as text on the site, with four steps, four prompts to start from and its contents. The Code tab's "Copy for an AI assistant" copies
     the reference, then the contract: its page and address, the crate the compile service builds it as and whether that has the standards, whether it is the example or
     an edit, the source in a fence longer than any run of backticks in it, the compiler's messages about that very source (at most 20, the rest counted; those of an
     earlier version of the source left out, with a hint to compile again), and what to do (fix the errors, or what the person asks next), with a last line asking for
     answers in Simplified Chinese on the Chinese page. "Only the code and errors" copies the same without the reference, for a conversation that has it already. Where
     the browser refuses to copy, the page offers the same text as a file. The reference is bundled with the page's script rather than fetched when asked, as a browser
     copies to the clipboard only within the press that asks (Safari strictly), with no time to fetch it.
  4. **The site publishes it** with the llmstxt.org convention: `/llms.txt`, the docs' index with a line on each English page, the contract reference first;
     `/llms-full.txt`, every English page in one Markdown file, the site's own blocks made plain and its links absolute; and `/llms-contracts.txt`, the playground's file
     as the repository has it when the site is built. `_headers` serves them as `text/plain; charset=utf-8`, as Workers give a `.txt` file no charset and browsers
     and many HTTP clients then read UTF-8 as windows-1252 or Latin-1 (found after the first deploy, which showed `…` as `â€¦`), and with
     `Access-Control-Allow-Origin: *`, so that a tool in a browser can fetch them. The site's Worker builds on a change to `playground/src/ai/` too.
  5. **A tutorial and an SDK reference for people**, in English and Chinese: "Your first contract" writes a poll, compiles it on the playground's Counter page, deploys
     it, votes from the wallet, tests it natively and asks an assistant to change it; "The contract SDK" lists the SDK's every item, with the reference's declarations.
     The poll is a new example contract, `contracts/examples/poll`, on the plain SDK, so that it compiles on the Counter page with nothing the compile service lacks; it
     has native tests, and `crates/ksana-execution/tests/tutorial.rs` runs its module (built by `ksana-test-contracts`) in the real VM through the tutorial's steps.
  6. **Code the site shows from the repository stays the repository's**: a code block marked `from="<path>"` must be that file's text, or one of the code blocks of that
     file when it is Markdown, or `npm run check` fails; the tutorial's contract and test and the SDK page's declarations are marked so. The check also resolves the site's
     own absolute links (`https://ksana.network/...`) in its pages and in the reference.
- **Rationale**: an assistant writes a working contract when what it needs is in front of it, and the one thing it reliably reads is what the person pastes: one file,
  complete, with declarations rather than descriptions of them. Only tests that read the source keep such a file true; a reference people keep in step drifts with the
  first change nobody copies into it. The SDK's rustdoc is complete but not published, spread over its modules and silent on gas, limits and the playground. Copying the
  source with the compiler's messages makes the commonest question, why a contract does not compile, one press. The llms.txt files cost nothing to build, and are where
  assistants that browse look first.
- **Alternatives**: a Chinese reference (assistants read the English one as well, the code is English either way, and an answer in Chinese is one line away); an MCP
  server, or an assistant built into the playground (a service to run and pay for, where the person's own assistant does); fetching the reference at the press (the
  clipboard does not wait); generating the declarations from rustdoc's JSON (unstable output, and it has neither the rules nor gas nor the playground); the tutorial on
  one of the eight examples (each does more than a first contract should, and a poll uses storage, the caller, events and reverts in under 150 lines).
- **Costs and limitations**: a change to the SDK, the gas table, the limits, the playground's examples or the compile service's limits fails tests until the reference
  says it, by design. The tests check declarations, values, examples and the playground's facts, not every sentence: the prose was checked against the code and SPEC once,
  by hand. The reference takes some 20 000 tokens of an assistant's context, which a small free tier may not take pasted whole: the download can be attached as a file
  instead, and the Code tab's second button serves a conversation that has the reference already. The playground's script grows by its 70 KiB, about 20 KiB compressed. The site's copy is the one built last, so it waits for the
  site's Worker to build. An assistant can still be wrong; the reference asks it to say what it chose where a request left a choice open, and the playground's compiler and
  read-only tries catch much of the rest before anything is signed. No new dependency.
- **Impact**: `playground/src/ai/` (new: `ksana-contracts.md`, `reference.ts`, `AiPage.tsx`, `reference.test.ts`), `src/ui/Code.tsx` (the Code tab's copy), `src/ui/ui.tsx`
  (`saveFile`), `src/app/App.tsx` (the route, the navigation, the home page), `src/app/workspace.ts` (`checked`: the source the diagnostics are of), `src/styles/app.css`,
  `src/i18n/en/core.ts` and `zh-CN/core.ts`, `playground/README.md`; `contracts/examples/poll` (new) and `contracts/Cargo.lock`; `crates/ksana-contract-sdk/tests/reference.rs`
  and `tests/reference/` (new), `crates/ksana-execution/tests/reference.rs` and `tutorial.rs` (new), `crates/ksana-test-contracts` (the poll's module); `site/lib/llms.mjs`
  (new), `site/build.mjs` (the llms files, `_headers`, the checks of `from` blocks and own links), `site/lib/markdown.mjs` (`from`), `site/site.config.mjs`, the pages
  `first-contract.md` and `contract-sdk.md` (new) and `contracts.md`, `host-functions.md`, `example-contracts.md`, `playground.md` and `index.md` in both languages, the
  Chinese heading font's subset, `site/README.md`; CLAUDE.md; PROGRESS M43.

## D-125 The landing page tells the life of an operation in six acts that the scroll plays, each a field of one of the brand's colors (2026-10-26, asked by the project owner; the site's landing page only; no change to the docs' pages, the wallet, the node or consensus, M44)

- **Problem**: the project owner asked for the landing page to be redone in the manner of the sites Awwwards features: rich in color, telling its story through
  form, beautiful and premium. The page was a monograph of numbered sections between hairlines on one paper ground, illustrated by the three marks and the rails
  figure: correct and calm, but its one moving idea, an operation going from a signature to final on Bitcoin, was told in text and still figures, and the brand's three
  colors appeared only as small marks.
- **Decision**:
  1. **A story in six acts after the prologue**, one mechanism of the protocol each, in the order an operation meets them: ten minutes (one Bitcoin block holds
     120 fast-lane blocks of five seconds; the chain's name, from the Abhidharmakośa), the fast lane (blocks about every five seconds while there are operations, blocks
     found at the same moment merged by the next, the anchor), settlement (the commit and reveal pair carrying the blocks' data in a Taproot witness into the next
     Bitcoin block), finality (decided final at six confirmations of its data, reported final when that decision has six of its own, the eleventh, D-114, D-116),
     two ways in (path B within seconds; path C forced in once confirmed, a block leaving it out invalid; top-up) and verification (the nodes gone, the indexer
     rebuilding the chain from Bitcoin alone and checking its state roots). The acts say what the docs say (how it works, confirmations, the wallet, KSA) and nothing
     more; the block share stays unmentioned (D-113).
  2. **Scrolling is time**: each act is a field of one of the brand's colors, paper, qing for the instant, ink for what is written to Bitcoin and night for
     verification; while an act is on screen its stage holds the screen (`position: sticky`, the act 2.5 to 3.3 screens long) and the scroll plays its drawing
     forward, its paragraphs taking turns beside it. The scroll itself is never taken over: the page scrolls as any page does, and the drawing follows the scroll
     with a lag of about 90 ms so that a wheel's steps play as motion. When the operation becomes final the seal stamps on its Bitcoin block and its vermilion floods
     the stage. Through acts 2 to 4 a bar at the bottom of the screen shows the operation's mark and age: signed, fast-confirmed at 4.6 s, on Bitcoin at 10 min,
     six confirmations at about 1 h, final at 1 h 51 min. The top bar takes the color of the field under it.
  3. **One drawing per act, written in its final state**: `lib/scenes.mjs` draws each scene as inline SVG, with its labels as HTML laid over it, and writes on
     each moving part, as data attributes, between which points of the act's progress it fades, grows, is drawn, moves, squares its corners or stamps.
     `src/js/landing.js` reads them and takes the drawing back to where the scroll stands. The drawing as built is what a reader sees with reduced motion, without
     scripts or in a browser without the `scripting` media feature, where the acts are ordinary sections with every paragraph showing.
  4. **What a visitor comes back for stays**: the three marks, now after finality as its summary; the reasons, on night; KSA with its supply curve; building;
     running it; where the project stands; the coda. The hero keeps its dial, with the signet's live readings under it, and lists the six acts below as chapters,
     each with its field's color. The rails figure and the two paths' cards leave the landing page, whose acts tell the same; the docs keep the rails figure.
  5. **Nothing new to load**: no library, no image, no font beyond the Chinese heading subset's new characters (the act titles, the chapters, the quotation);
     the content security policy is unchanged.
- **Rationale**: the protocol's own shapes are the brand's (the circle for the instant, the square for what is written, the seal for what is final), so a story
  drawn with them explains the system while it colors the page, and what moves is what happens. Pinning by sticky positioning with a progress read from the scroll
  keeps native scrolling, anchors, the browser's find and assistive technology working, which a page that takes the scroll over breaks. Markup in its final state
  makes the page without scripts or motion complete by construction rather than a second design to keep in step.
- **Alternatives**: a WebGL or canvas scene (heavier, empty without the script, and its labels would leave the HTML); GSAP's ScrollTrigger or a smooth-scroll
  library (a dependency, and a script from outside the site's origin that the content security policy forbids, for what sticky positioning and a few hundred lines
  do); video (heavy, untranslatable, not in step with the scroll); keeping the monograph and adding color (the owner asked for form telling the story).
- **Costs and limitations**: a reader who scrolls the whole page scrolls about 23 000 px at 1 440 × 900 (13 000 without pinning); the docs, the wallet and the
  quickstart are in the top bar, the hero and the coda, and the hero's chapters jump to any act. The landing page's HTML grows from 95 KB to 139 KB (21 to 31 KB
  compressed) and its script from 6 KB to 19 KB (6 KB compressed). The drawings are fixed compositions of 600 to 800 units scaled to the screen: on a phone 375 px
  wide they are 340 px wide, and their labels 9 to 11 px. Text keeps at least 4.5:1 against every part of a field it can stand on, the vermilion's lighter centre
  included. A browser without the `scripting` media feature (Safari before 17) gets the sections without pinning.
- **Impact**: `site/lib/scenes.mjs` (new), `site/lib/landing.mjs`, `site/content/en/landing.mjs` and `zh/landing.mjs` (the acts, the story bar, the chapters;
  the rails and paths copy removed), `site/src/css/07-landing.css` (rewritten), `site/src/css/09-figures.css` (the rails' and the supply curve's styles, which the
  docs use, moved from 07), `site/src/js/landing.js`, `site/build.mjs` (the chapters and the quotation in the Chinese heading font's text), the Chinese heading
  font's subset, `site/README.md`; CLAUDE.md; PROGRESS M44.

## D-126 Testnet points: a page of its own and a service beside the signet that scores tasks from the node's blocks once they are BTC-final (2026-10-26, asked by the project owner, who chose season 1's staking as the holding time of liquidity and lending deposits; a new directory `points/`, a service in the signet's Compose project, the signet's official instances, the web wallet's top-up page opened with a lock in its address; no consensus, VM, node, RPC or SDK change, M45)

- **Problem**: the project owner asked for a site that scores what people do on the public signet, as one input to a retroactive airdrop when the mainnet launches: a single
  social task (link an X account by posting a tweet) and tasks across the network's features (transfers, contracts, interaction, the AMM, lending, staking and the rest).
  They approved the plan on 2026-10-26 and chose to count season 1's staking as the time liquidity and lending deposits are held, with no staking contract. What shapes the
  design: points must follow from chain data anyone can check; an op can only count once it can no longer be taken back; the signet restarts from genesis when consensus
  changes, and points must survive that; making accounts and ops costs almost nothing (the relay floor is 1 base unit, the faucet gives 100 KSA a day per account), so
  counts and amounts cannot be rewarded; and nothing the page says may promise a token (the earlier public posts never mentioned a mainnet, a token or a sale).
- **Decision**:
  1. **A page and a service, apart from the wallet**: `points/` is a package of its own, like the playground (D-119): a React page in English and Simplified Chinese
     (English unless `?lang=zh-CN` or the reader's choice says otherwise, D-115), served by a Cloudflare Worker `ksana-points` with static assets only, and a Node.js 22
     service (`points/server/`, TypeScript bundled into one module with the SDK and the playground's ABI parser) in the signet's Compose project, profile `points`, behind
     Caddy at `/points` of the signet's site. The page calls the service across origins, allowed for the page's origin alone, as the faucet and the compile service allow the
     playground's (`setup --points <origin>`). The service keeps everything in one SQLite database (`node:sqlite`, Node's standard library) on a volume of its own, which
     `backup-data` saves with the seed nodes' volumes. Until the project owner approves the public launch the page runs only at its `workers.dev` address, marked as a
     preview, linked from nowhere and kept out of search engines.
  2. **Scored from the node, credited at BTC-final**: the service reads a node's JSON-RPC over the Compose network (the seed node the faucet uses, node2 when there are two,
     not the public `/rpc`). It walks the main chain
     height by height with `ksana_getBlock`: the blocks each main-chain block merges, in the order of `merged`, then the main-chain block, each entry with its receipt, which
     is the order of `position`s (D-111). It keeps one row per op (position, op ID, signer, path, the action without a deployment's code, the signature scheme, the outcome,
     the events and the asset changes) and one per block (hash, producer, timestamp), by the hash of the chain's genesis block. Only ops that succeeded count, and only once
     their height is at or below the BTC-final tip of `ksana_getChainInfo` (D-114); above it they are shown as pending. The block hash kept for each height tells a main-chain
     switch: the rows above the fork are dropped and scanned again. A BTC-final tip taken back (D-114) does the same and recomputes what was credited from the rows. The
     block's carrier comes from `ksana_getSettlement`. The indexer lists no ops; its `indexer_status` state root is compared with the node's header at the same height, and
     crediting stops with an alert while they differ.
  3. **Contracts by code hash, events by their ABI**: a deployment's contract ID is computed as the execution computes it (`sha256(deployer ‖ nonce ‖ code hash)`, SPEC
     §9.2). A contract whose code hash is in `playground/src/examples/wasm/builds.json` is that example, and its events are decoded with the example's `abi` block by the
     playground's own parser and codec, bundled into the service. A contract's first `standards` event declares KS-20 or KS-721, and its standard events count from there on
     in exactly the standard's layouts (STANDARDS §2). The official instances, the pool, the lending market, the poll, the NFT collection and the exchange of top-ups, are
     named in `signet/points.json` under the hash of the genesis block they are on, as the playground's `deployment.json` names its shared instances (D-119).
  4. **Tasks once per account, rules versioned**: 25 tasks in eight groups (`points/shared/tasks.ts`), each completed once by the op that first meets it, which the page
     links as its proof, the last of them the days active:

     | Group | Task (points) | Judged from |
     | --- | --- | --- |
     | Social | Post the verification tweet, linking X (30) | point 7 |
     | Getting started | Get KSA from the faucet (10); send KSA (20); find your op's Bitcoin transaction (20) | a transfer from a faucet account; a KSA `AssetTransfer` to another account; point 7 |
     | Bitcoin-native | Submit an op through Bitcoin, path C (60, then 10 for each later calendar week with one, up to 4); buy KSA with signet BTC, a top-up (80) | a forced-inclusion entry; a `payout` of KSA from an official exchange |
     | Assets and tokens | Issue a built-in asset (30); deploy a KS-20 token (40); send a KS-20 token (30); list an NFT for sale or buy one (40) | a `create` asset change; a deployment declaring KS-20; a KS-20 `transfer` from the signer; a `list` or `sale` event of the NFT example |
     | DeFi | Swap in the official pool (30, then 10 for each later calendar week with one, up to 4); provide liquidity to the official pool and hold it (40, then 2 a day, up to 30 days); supply the official lending market and hold it (40, then 2 a day, up to 30 days); borrow and repay (50); liquidate a position (80) | the official instances' `swap`, `add_liquidity` / `remove_liquidity`, `supply` / `withdraw`, `borrow` then `repay`, `liquidate` events |
     | Contracts | Deploy an example from the playground (20); deploy a contract you wrote or changed (60); your contract used by others (5 for each other account calling it, up to 100); execute a multisig proposal (40); create a vesting schedule and release from it (40); submit an op with `ksana-cli` or the SDK (30); vote in the official poll (10) | a code hash in `builds.json`, or none known; direct calls to a contract the account deployed; `execute`; `create` then `release`; a BIP340 signature (browser wallets sign BIP322); `voted` |
     | Running the network | Produce blocks (150, then 10 for each later day with a block, up to 30 days); carry another producer's block to Bitcoin (100) | a block header's `producer`; a block's `carrier_account` that is not its producer |
     | Activity | Each UTC day with an op of yours that succeeded (5, up to 60 days) | |

     Days and weeks are UTC dates of the block timestamps (weeks Monday to Sunday). The rules carry a version; the service recomputes every account from the kept rows when
     it changes, so the points are a function of the rules' version and the chain. The most an account can reach is about 1 950: an ordinary participant who plays the season
     through reaches roughly 600 to 900, and the social task is a small part of it.
  5. **Season 1's staking is holding**: the shares of the official pool (`add_liquidity` less `remove_liquidity`) and of the official lending market (`supply` less
     `withdraw`) by account; the first deposit completes the task, and each UTC day whose end finds the account's shares at or above the instance's floor
     (`signet/points.json`) adds 2 points, up to 30 days. No staking contract: Ksana is proof of work, and the site's mining page says it is not proof of stake.
  6. **Accounts, partitions and exclusions**: an account is a Taproot output key (D-092), so a signet `tb1p` address and a mainnet `bc1p` address of the same key are the same
     account. A restart of the signet from genesis opens a new partition and keeps the old one's points; a task completed on both counts once and days count once by date.
     The faucet, the operator's account, the block share account, the seed nodes' producers, the keeper, the top-up seller and the team's test accounts
     (`signet/points.json`) earn nothing and do not count as other accounts.
  7. **Sign-in, X and the Bitcoin transaction**: signing in is a BIP322 signature (Unisat, OneKey, Xverse, through the SDK's adapters) of a message the service issued with its
     domain, the account, a nonce and an expiry, checked with the SDK's `verifyBip322ForAccount`; the service answers with a token the page keeps and sends as a bearer
     token, and stores only its hash. The social task is one tweet that carries a code the service gave the signed-in account, never the address, so that the tweet does not
     tie the X account to the address in public; the service reads the tweet's author and text from X's public oEmbed endpoint, which needs no key, and one X account and one
     tweet belong to one account. The Bitcoin-transaction task is a txid the signed-in account submits: the reveal or commit transaction of a confirmed settlement that
     carries one of the account's ops (`ksana_getSettlement`). With a Turnstile secret configured, signing in also needs Cloudflare Turnstile's token.
  8. **What the page shows and keeps**: the tasks with their status and proofs, how to do each (the wallet, the playground, the docs), an account's points and the share of
     accounts it is ahead of, for any address; no ranking table. The X account linked to an address is shown to the signed-in account alone. The service keeps an account's
     X handle, tweet and sign-in sessions until the account deletes them; client addresses only in memory, for rate limits. The page says that points have no monetary
     value, cannot be transferred or exchanged and give no right to any token or payment, and that the project has made no commitment to any program; the wording the
     project owner approves before the launch replaces the draft.
  9. **Snapshots and duplicate accounts offline**: `node points.mjs export` writes each account with its points, the rules' version and the op IDs of its tasks, and the first
     account that funded it with KSA; grouping the accounts one person controls, by funding and by behaviour, happens on that export before any distribution, and its
     thresholds are not published.
  10. **The official instances and their price keeper**: `node points.mjs instances` (`signet/deploy.sh <host> instances`) deploys them once per chain from the operator's
     account, with the playground's shipped modules of the mintable token, the AMM and the lending market and the poll's module built in the points image: the token Wizards
     (WIZ, 8 decimals, 10 000 000 minted to the operator), a pool of KSA and WIZ with 100 000 KSA and 1 000 000 WIZ, a market lending KSA against WIZ with the playground's
     parameters, priced at the pool's price and supplied with 200 000 KSA, 1 000 KSA for the keeper's fees, the market's admin role handed to the keeper, the season's
     poll, the KSA exchange (the ksa-exchange example, built in the points image) and 100 000 KSA for the top-up seller (point 11). Each step's op is recorded with its nonce before it is sent (`config/points/official-<genesis>.json` on the server), so a run cut short follows the op it sent,
     signs the step again at the same nonce when the node lost it, and never deploys twice. The staking floors are the shares that held 10 KSA of the pool's KSA side, or
     of the market's deposits, at deployment. Ksana has no price oracle and the lending example's price is its admin's to set, so the keeper, a key of its own on the server (`keys/keeper.key`), reads
     the pool every 10 minutes and, when the pool's price of WIZ is 1% or more away from the market's, sets the market's price towards it by at most 20% a round.
  11. **A standing seller of top-ups**: a top-up starts with a seller locking KSA for the buyer's account (D-085), and a visitor has nobody to ask, so the service sells
     them itself when `KSANA_POINTS_SELLER_KEY_FILE` names a key of its own (`keys/seller.key` on the server, mounted into the points service alone). A signed-in account
     asks the page for a lock (`POST /topup`); the seller locks 100 KSA for it on the official exchange, all of it for 1 000 sats, expiring 144 BTC blocks above the height
     the node's tip anchors at, and the page opens the web wallet's top-up page with the exchange and the lock in its address (`#/wallet/topup?exchange=<contract>&lock=<id>`),
     where one transaction pays the lock and carries the claim (path C). Each lock is paid to an address of its own, the BIP86 Taproot address of the key
     `taggedHash("KsanaPoints/top-up", seller's key ‖ genesis block hash ‖ u64le(nonce of the op making the lock))`: never used twice (the exchange refuses a script it has
     seen) and the seller's to spend. The requests are rows of the service's database, handled in order with one op of the seller's in flight: a lock's op is recorded
     with its nonce before it is sent, signed again at the same nonce when the node lost it, and when another op took the nonce the lock is looked up with the exchange's
     `lock_of_script` and checked for its seller and beneficiary, so a request never makes two locks. An account asking again gets the lock it has; from 12 BTC blocks
     before the expiry the page shows it as expired, its deadline to pay (the wallet refuses within 3 blocks of Bitcoin's tip, which the anchored height lags), and the
     account can ask for another. A lock nobody paid in full is released 6 BTC blocks after the contract's `RELEASE_DELAY` allows it, and the next lock takes the seller's
     free balance on the exchange before any KSA is attached. At most 100 locks are open at once; none is offered while the seller's KSA, less 1 KSA kept for its fees,
     cannot fill one, nor to an account that topped up already or is one of the project's own.
- **Rationale**: the node keeps every main-chain block with its receipts on disk (D-111), so the service needs no state of its own that the chain cannot rebuild: it can
  scan again from genesis at any time, and a rules change is a recomputation, not a migration. Crediting at BTC-final means a credited task is never taken back but by the
  BTC reorg of at least six blocks that takes back finality itself. Recognizing contracts by code hash and decoding their events by the `abi` block the playground already
  parses keeps the scoring in step with the examples without a second description of their events. Rewarding variety and days, each capped, bounds what one account can
  earn, so that farming takes many accounts kept active for weeks, and the grouping of accounts before any distribution can merge them. A page of its own keeps the wallet's
  origin, where top-up claims are kept, apart from a page many people will be sent to.
- **Alternatives**: the indexer as the data source (it executes blocks but answers only its status and state roots); a points contract on chain (every task would need a
  contract call, and the chain cannot see X); listing each account's ops with `ksana_getAccountOps` when it signs in (simpler, but blocks' producers and carriers, which no
  op lists, and every account that never signs in would be missed); a staking contract (a new example with a reward token, left for later); X's paid API for every tweet
  (oEmbed answers the same for a public tweet without a key; the API stays an option for an X account's age); a page inside the web wallet (its origin keeps top-up claims,
  and its release follows the wallet's); PostgreSQL or a new SQLite package (a second server or a new dependency, where Node 22 has SQLite built in).
- **Costs and limitations**: `node:sqlite` is still marked experimental in Node 22 and prints a warning at start, which the service turns off; its API is small and the
  service uses little of it. oEmbed gives the author's handle, not the account's numeric ID, so a renamed X account could be linked again; the snapshot's review checks the
  tweets again. Holding days are measured at the end of each UTC day by the block timestamps, which producers set within the consensus bounds. The service follows one node,
  so the points follow what that node takes for the main chain; the indexer's state roots guard against a node that went astray. The official market is only as honest as
  its keeper, a key the project holds: its price, which liquidations follow, is the project's to set; the keeper follows the pool, whose price a large swap moves, and its
  steps of at most 20% every 10 minutes slow such a move down without preventing it, which is acceptable on a test network with test KSA. The seller's open locks are
  bounded (100 locks of 100 KSA) but who asks is not: accounts cost nothing, so many of them can keep every lock taken for a day, and the page then answers that the
  seller is busy; a top-up from a lock of one's own on the official exchange, made with a second account, counts all the same. The signet BTC paid to the seller's
  addresses stays with its key; nothing spends it yet. Locks made ahead for whoever comes would need no request, but the exchange fixes the beneficiary when it locks.
- **Impact**: `points/` (new: the page, the service, their tests and README), `signet/points.json` (new), `signet/docker-compose.yml`, `signet/Dockerfile`, `signet/Caddyfile`,
  `signet/deploy.sh`, `signet/server.sh`, `signet/ksana-signet.sh` (the keeper's and the seller's keys), `signet/rehearse.sh`, `signet/rehearsal.yml`, `signet/.env.example`,
  `signet/README.md`, `playground/hosted/deployment.json` (the shared instances, once deployed); `web/` (the top-up page's address, `docs/WEB_DESIGN.md`); CLAUDE.md (the
  `points/` directory, its commands and its Chinese language pack); PROGRESS M45.
- **Revised by D-127** (M45): an address takes part only while it has an X account linked, so the social task of point 4 is the way in rather than one task among
  the others; the groups are listed with the Bitcoin-native tasks after the contracts; the rules are version 2.

## D-127 Testnet points: an address takes part only with an X account linked, and the Bitcoin-native tasks come after the fast lane (2026-10-27, asked by the project owner before the service was deployed; revises D-126 points 4, 7 and 8; the points service and page, the signet's rehearsal; no consensus, VM, node, RPC or SDK change, M45)

- **Problem**: looking at D-126 before its deployment, the project owner asked for two changes: every address must link one X account before it may take part, and a
  newcomer must not meet path C first, as the slowest way in is the wrong first contact with the network. In D-126 the tweet was one task of 30 points among 25, any
  account with a credited op had points and a standing, and the page listed the Bitcoin-native group, path C and the top-up, right after getting started.
- **Decision**:
  1. **Taking part**: an account takes part when it earns points (point 6 of D-126) and has an X account linked (`Engine.takesPart`). Crediting does not change:
     every account's tasks are credited from the chain as before and its view shows them, with what they give. Only an account taking part has a score, the table the
     standing, the season's count of accounts and of each task, and the snapshot are taken from, so an account that never links X is in none of them, and its view
     answers `taking_part: false` with no standing. Linking X writes the account's score at once from everything credited so far, so what the address did before it
     linked counts; unlinking, or deleting its data, removes the score again. One X account still links one address and one tweet one account (point 7 of D-126).
  2. **What waits for the link**: the settlement claim and the top-up need it (`403 x_required`); linking X is the one claim an account makes without it. The page says
     so in the hero, marks the social task as required while it is not done, shows a signed-in account that does not take part its points greyed with a button to
     the X task, shows another such account as not taking part, and in place of the settlement claim's form and the top-up's lock says to link X first. Unlinking
     asks for confirmation, as it takes the address out.
  3. **The order**: the groups are listed social, getting started, assets and tokens, DeFi, contracts, Bitcoin-native, running the network, activity. The
     Bitcoin-native note says that these tasks pay in signet BTC and wait for Bitcoin blocks, minutes rather than seconds, and are best tried once the fast lane is
     familiar. Getting started keeps the settlement transaction, which needs no signet BTC, now saying that a Bitcoin block confirms it within minutes.
  4. **Rules version 2**: who takes part changes the scores, so `RULES_VERSION` is 2 and a service that finds version 1 in its database recomputes every account
     from its rows. The snapshot's X handle stays the social task's detail.
- **Rationale**: tying participation to an X account, one to one, makes each participant cost an X account, which raises the price of farming with many addresses
  far more than the 30 points the tweet gave; points were never meant to count without it. Counting what an address did before linking keeps the testnet's early
  users and the order of a visit free: one can try the wallet first and link later, and nothing a public chain already shows has to be done again. Gating the score
  rather than the crediting keeps the credit a function of the chain and the rules alone, so linking and unlinking change no credited row and need no
  recomputation. The settlement claim and the top-up are the two steps the service performs for an account; the top-up spends the seller's KSA, which accounts that
  do not take part should not take. Path C and the top-up are the two tasks that need signet BTC from a faucet and wait for Bitcoin blocks; after the fast lane a
  visitor has KSA, has seen an op final within seconds, and knows what the slower path adds.
- **Alternatives**: crediting only after the link (an address would have to do everything again after linking, and every link would be a recomputation); hiding
  an unlinked account's points (it would no longer see what linking would give it); a larger reward for the tweet instead of a gate (it would still leave the
  points of unlinked addresses in the standing and the snapshot); moving find-the-settlement-transaction to the Bitcoin-native group as well (it is fast for the
  user, an explorer lookup, and introduces the settlement every op has).
- **Costs and limitations**: one X account per address limits farming only as far as X accounts are hard to come by; oEmbed's author handle, not the account's
  numeric ID, is what is unique (point 7 of D-126, unchanged), so the snapshot's review still checks the tweets. Whether an address takes part is public, though
  not which X account it linked. An address that unlinks leaves the counts at once and comes back with the same points when it links again.
- **Impact**: `points/shared/tasks.ts` (version 2, the order), `points/shared/api.ts` (`taking_part`), `points/server/` (`engine.ts`, `score.ts`, `store.ts`,
  `api.ts`, `x.ts`, `export.ts`, `main.ts`: `KSANA_POINTS_X_OEMBED` points the service at a stand-in for X's oEmbed endpoint) and their tests, the page (`src/ui/`,
  both language packs, the styles), `points/README.md`; `signet/rehearse.sh` and `signet/rehearsal.yml` (the buyer is refused a lock, links X through a stand-in for
  oEmbed and then tops up; the faucet's recipient does not take part); CLAUDE.md; PROGRESS M45.

## D-128 The site lets the beacon of Cloudflare's Web Analytics run (2026-10-28, chosen by the project owner; revises D-113 point 4; the site's content security policy only, M44)

- **Problem**: the zone's Web Analytics is on with its automatic setup, so Cloudflare adds its beacon to every HTML page it serves a browser
  (`<script type="module" src="https://static.cloudflareinsights.com/beacon.min.js/v…" integrity="sha512-…" data-cf-beacon="…">`). D-113 kept the site free of
  external scripts with that setup off, and the site's content security policy blocked the beacon: each page logged an error in the console, and the statistics
  received nothing. Found on 2026-10-26 while checking M44's deploy; asked whether to turn the setup off or to let the beacon run, the project owner chose on
  2026-10-28 to let it run.
- **Decision**:
  1. The policy `site/build.mjs` writes into `_headers` adds `https://static.cloudflareinsights.com` to `script-src`, and nothing else. With the automatic setup
     the beacon reports to `/cdn-cgi/rum` on the page's own origin (Cloudflare's Web Analytics FAQ), which `connect-src 'self'` already allows.
  2. D-113 point 4 is revised: the pages load one external script, the beacon that Cloudflare's edge adds to them. The site's own files still reference nothing
     from another origin, and the landing page's live strip stays the only request the site's code makes to one.
  3. The zone's other hosts need nothing: the contract playground's policy sets only `frame-ancestors`, so its pages ran the beacon already, and the signet's web
     page is served without it.
- **Rationale**: the project owner wants the visits counted. The beacon comes from the company that already serves every byte of the site, and could change any
  page it serves, so allowing its host adds no party the site does not trust already. The beacon sets no cookie and keeps nothing in the browser's storage.
- **Alternatives**: turning the automatic setup off, as D-113 had it (no external script, and no page statistics); allowing only the path
  `https://static.cloudflareinsights.com/beacon.min.js/` (it matches the versioned path the beacon has today, but Cloudflare can move it and the policy would block
  the beacon again without a sign; the host serves Cloudflare's beacon); a snippet written into the pages by the build (a token in the repository, reports to
  `cloudflareinsights.com`, and the same script).
- **Costs and limitations**: every page loads the beacon from Cloudflare's host, about 32 KB (10 KB compressed), with the integrity hash Cloudflare writes into the
  tag; a new version comes with a new path and hash, which the host-wide source follows. Each report tells Cloudflare the page's address and referrer, the
  browser's engine and version, its memory use and the page's loading and paint timings. A browser that blocks `static.cloudflareinsights.com` sees the page as
  before, and its visit is not counted.
- **Impact**: `site/build.mjs` (the policy), `site/README.md` (deploying); CLAUDE.md; PROGRESS M44 (a follow-up).

## D-130 Entry-pool churn no longer keeps producers rebuilding their templates: a replacement raises the fee by a tenth, only a change of the packing candidates counts, and a pool change waits for four times the template's build (the second internal audit's A2-08, 2026-10-28; revises SPEC §6.2, §8.2, §14; local policy, no consensus change, M47)

- **Problem**: the second internal audit's A2-08. Every op admitted to the entry pool, replaced or evicted moved the pool's generation, and the producer's
  solve loop, which checks between PoW attempts of about a millisecond (16 Argon2id nonces, D-094), threw its template away at the next check and built
  another: GHOSTDAG on the tip, a state read for every account in the pool under the pool's lock, the merged blocks' execution. An op behind a nonce gap is
  never packed and never pays, yet it was admitted, relayed to every node and could be replaced for one more unit of fee over and over, so anyone with a
  funded account could keep every producer rebuilding for nearly nothing; a busy honest pool had the same effect. F-12 (M26) bounded how many such ops an
  account keeps (`max_gapped_per_account`), not how often they change.
- **Decision** (all local policy):
  1. A replacement pays at least `Policy::replacement_fee(old)` = old + max(⌈old × `replacement_bump_percent` / 100⌉, relay floor, 1), with
     `replacement_bump_percent` 10: a raise of a tenth, and at least what admitting a new op asks, as BIP125 asks a replacement on BTC to pay for its own
     relay. The refusal stays `-32011` and names the least fee: `an op with the same nonce already exists; replacing it needs a sponsored fee of at least
     <fee>`.
  2. The generation moves only for a change that can alter the packing candidates. `insert` compares the account's run of candidates (from its nonce in
     the state the op is admitted against, up to the first op below the packing floor) before and after, and an eviction for the pool's op count or byte
     limit counts when it takes an op in its account's run. An op behind a nonce gap or after one below the packing floor, its replacement and its eviction
     leave the generation alone. `remove`, `remove_committed` (when it removes anything) and the new `mark_changed` move it as before. The node calls
     `mark_changed` when the main-chain tip moved between reading the state an op is admitted against and admitting it, so that a template built on the new
     tip is checked again and `ksana_submitOp` still answers once the op is in the template.
  3. The producer's solve loop takes a change of the generation as making its template stale only once the template has been solved for
     `POOL_REFRESH_RATIO` (4) times as long as building it took. A new main-chain tip, a change of the block DAG, a new BTC tip and `TEMPLATE_LIFETIME`
     (2 s) end it at once, as before.
  4. The producer keeps the account nonces it read for the packing candidates, by main-chain tip (`ProducerCache`, through `Mempool::candidates_by`): a
     template built again on the same tip reads the state only for the accounts new to the pool.
- **Rationale**: point 1 makes a run of replacements grow the fee geometrically. An account's KSA must cover the fees of all its queued ops, so a balance
  allows a number of replacements of one op that grows with its logarithm, not with the balance itself (`ksana-mempool/tests/pool.rs`: 72 instead of 9 888
  from a fee of 2 with 10 000 base units), and each one pays at least what a new op pays to be relayed. Point 2 takes away the free part of the attack: what
  moves the generation is an op a block takes and pays for, or the removal of one. Point 3 bounds the rest, whatever its source (an attacker paying for real
  candidates, a busy honest pool, many accounts): rebuilding for pool changes takes at most a fifth of the producer's time, however fast the candidates change.
  Scaling with the build time keeps a new op's wait short while templates are cheap (a few milliseconds), and the lifetime caps it at 2 s while they are not.
  Point 4 removes the state reads that were most of a rebuild's cost under the pool's lock, and is exact, since the state of a tip never changes.
- **Alternatives**: a fixed delay of a few hundred milliseconds, as the audit suggested (the same bound for an expensive template, but a longer wait for every
  op on a cheap one, and no bound on the share of the time once building takes longer than the delay); refusing ops behind a nonce gap (honest wallets send
  ops out of order, and F-12 kept room for them); counting replacements by account or by peer (more state to keep, evaded with more accounts); only BIP125's
  rule that the fee rises (+1 already met it).
- **Costs and limitations**: a replacement costs a tenth more (and at least the relay floor more), and a wallet that replaces a waiting op must raise its fee
  by that much; the refusal names the least fee. A new candidate reaches a running template up to four times its build time later than before (2 s at most),
  and `ksana_submitOp` waits that much longer before answering. Someone who pays for ops that blocks take can still keep a producer rebuilding up to a fifth
  of its time. An eviction is judged in the state the evicted account's nonce was last read in: between a main-chain change and the pool's re-check of it, a
  running template can keep an op the pool evicted, which a block then includes and is paid for as before. A change of the block DAG that leaves the
  main-chain tip as it was still makes the template stale at once: that is the residual of the second audit's A2-07, left to that finding.
- **Impact**: `crates/ksana-mempool` (`Policy::replacement_bump_percent` / `replacement_fee`, the generation, `mark_changed`, `candidates_by`, tests);
  `crates/ksana-node` (`producer.rs`: `POOL_REFRESH_RATIO` and the nonces kept by tip; `ops::admit`; a test in `tests/adversarial_liveness.rs`); SPEC §6.2,
  §8.2, §14; the second audit's report (A2-08, F-12); the site's pages on sending operations and the CLI; CLAUDE.md; PROGRESS M47.

## D-131 A pair whose P2A output another party's child holds goes out again in a new pair of this node's (the pinning residual of the second internal audit's A2-09, 2026-10-28; revises D-102 items c and d and its limitations, D-117's limitations and SPEC §7.2, §7.3, §7.7, §14, §15; local policy, no consensus change, M48)

- **Problem**: D-102 left P2A pinning open, and D-117 found that it holds a pair the room misjudged too. A reveal is a version 2 transaction whose P2A output anyone can spend, so anyone can
  attach a large child at a low rate first (up to the mempool's descendant limits, about 100 kvB). A child of this node's must then pay more than that child's whole fee to replace it (BIP125
  rule 3, which Bitcoin Core keeps with its cluster mempool), so `testmempoolaccept` refused it every round and the pair stayed at the pinned package's rate. A lone producer never rescues its
  own waiting settlement (D-102 item d), so its block's data waited for as long as the pin held, and after `SETTLEMENT_TIMEOUT` the settlement backlog limit stopped path B on every node; a
  rescue or a republish of this node's could be pinned the same way. The attacker's child costs nothing while the package stays below what miners take. The adversarial review of 2026-10-28
  listed it as the open part of A2-09.
- **Decision**:
  a. When bitcoind refuses the first CPFP child the bump of D-102 item c built for a pair this node sent (no child of this node's is in flight for the pair), while the reveal waits in the BTC
     mempool and its P2A output is spent there (`gettxout` with the mempool has nothing for it), the output is held by another party's transaction. The pair's payload then goes out again,
     byte for byte, in a new commit / reveal pair funded by this node at the rate the bump aimed for (`Rescuer::send_again`), assembled and checked as a rescue is (the checks of attached
     settlement transactions, then `testmempoolaccept`), its funding reserved before it is broadcast.
  b. The new pair takes the old one's place: for a block's data (the block's own settlement, or a rescue of it) as this node's rescue of the block, for a republish as the republish from its
     first header. The funding inputs of the pair it replaces, the block's own settlement included, stay reserved while any of its transactions may be in the BTC mempool (D-104): a pinned
     pair can wait there long after the settlement record of its block, which reserves the inputs of the block's own settlement, has closed, and the producer would otherwise pick them for a
     block whose settlement then conflicts with the pinned pair, and discard the block it had solved. The new pair is kept alive and bumped like any rescue or republish, and goes out again in
     turn when it is pinned as well.
  c. Once a block's own settlement has gone out again, the producer no longer bumps it; the new pair is bumped instead. Nothing more is spent on a pair whose block's carried data has all
     landed, through whatever transactions: it is not bumped (D-102 bumped the block's own settlement until its record closed, `BTC_FINALITY_CONFS` BTC blocks after the landing).
  d. The payload of a block's data goes out again only once the local BTC tip has reached the block's anchor, as for a rescue (D-100).
- **Rationale**:
  - The new pair spends nothing of the old one's (settlement transactions spend only confirmed funding outputs, D-075), so no transaction of another party's stands in its way: the
    pinning child holds the old pair, not its payload. The data lands with whichever pair confirms first, and a second landing is harmless, as for any rescue: the first landing counts
    (D-077, D-095), and the carrier is still the producer of the block that carried the original (§8.6).
  - Ignoring the old pair is cheap where replacing the pinning child is what rule 3 makes expensive: the new pair costs one pair at the bump's rate, which the node was about to pay for the
    pair and its child anyway, plus the old pair's fee should that confirm later too.
  - The check is narrow, so that an ordinary refusal sends nothing again: only the first child of a pair (the replacement of this node's own child is refused for other reasons), only while
    the reveal waits in the mempool and only when its P2A output is spent there. It costs two calls after a refusal (`gettxout`, `getmempoolentry`), and nothing otherwise.
  - Alternatives: TRUC (version 3) transactions, the usual defence against pinning, do not fit. The version of the attached pair and the outputs of the reveal are consensus (§7.2), and
    under TRUC an unconfirmed transaction has at most one unconfirmed ancestor and one unconfirmed descendant, and a child of an unconfirmed TRUC parent at most 1 000 vB, which leaves no room
    for commit → reveal → child (a reveal with a large envelope is over 1 000 vB by itself). Ephemeral anchors need a parent paying no fee, and package RBF replaces only packages of a parent
    and its child. Each would change the pair's shape, a consensus change, for a local problem. Leaving a pinned pair to other nodes' rescues, as D-102 did, waits out their patience with
    a waiting pair (`CLAIM_PATIENCE`, D-102 item d) and does nothing for a lone producer.
- **Limitations**:
  - The node keeps no record of its children across a restart: one of its own children still in the mempool then looks like another party's, and a child replacing it at the same rate is
    refused, so the pair goes out again once, `BUMP_AFTER` BTC blocks after the restart.
  - A child of another party's that pays a good rate (one helping the pair along) is treated like a pinning one: the pair goes out again although the package might have confirmed.
  - Each pin costs a pair at the bump's rate. The new pair pays the rate the bump aimed for, so it needs a bump of its own only when the rate rises (a fee spike, or D-117's doubling of a pair
    left waiting with room); an attacker who pins every new pair makes the node send one more pair for every `BUMP_AFTER` BTC blocks of rising rates, the rate bounded by the cap.
  - Other nodes judge the old pair by its own rate (D-102) and may still rescue in their turn; the new pair, at this node's rate, holds their rescues back as a claim does (D-098).
  - The mock chain has no RBF and refuses any conflict, so the node tests cover a pinned pair and not a replacement by fee. Only the pinned settlement was tried against a real bitcoind
    afterwards (Bitcoin Core 28.4 on regtest, 2026-10-29, `tests/integration/tests/m48_regtest.rs`): `gettxout` with the mempool has nothing for the taken P2A output, `testmempoolaccept`
    refuses a child of the producer's that pays less than the pinning child (`insufficient fee`, BIP125 rule 3), and the payload goes out again and lands; a pinned rescue and a pinned
    republish are tried on the mock chain only.
- **Impact**: local policy only, no new flags and no change to `ksana-btc`; `Rescuer::bump`, `Rescuer::send_again`, `Rescuer::anchor_taken`, `Rescuer::landed`, and `Rescuer::build_pair`, the
  rescue's assembly shared with the pairs sent again. Tests (node): a lone producer's settlement pinned by another party's child goes out again at the bump's rate after `BUMP_AFTER` BTC
  blocks, only once, and lands, nothing more is spent on the pinned pair, and its funding stays reserved after the block's record closed, until it leaves the mempool; a pinned rescue
  goes out again in its place, the pinned rescue's funding reserved while it waits; a pinned republish goes out again with the same headers; a producer's settlement whose data landed
  through another party's transaction is not bumped. Test (regtest, added later): the pinned settlement against Bitcoin Core's own mempool, as the limitations say. Documentation: SPEC's
  version history, §7.2, §7.3, §7.7, §14, §15; D-102 and D-117 (their limitations point here); CLAUDE.md; PROGRESS M48; the status of A2-09 in the second
  audit's report; the site's mining page.

## D-132 The P2P layer and the RPC servers bound what one source can make a node hold or do: connections by address, quotas that outlive connections, budgets for the gossip awaiting validation and for serving sync, op messages within `MAX_OP_BYTES`, RPC batches, bodies and subscriptions, and a `ksana_getAccount` linear in the window (the first internal audit's F-28 and F-29, 2026-10-28; revises D-040, D-059 and SPEC §11.1, §11.2, §14, §16; local policy, no consensus change, M49)

- **Problem**: the rest of the first internal audit's F-28 and its F-29, both still open in the second audit's report (M26 had only dropped the queued gossip of banned peers, D-098 h).
  In the P2P layer, with libp2p's defaults (D-040, D-059):
  1. Connections were not limited: any number of inbound connections, in the handshake or established, from one address or with one peer. D-059's token buckets belonged to a connected
     peer and were deleted when it disconnected, so a reconnection, or a new identity (a key costs nothing), started with full buckets: one host could take any number of quotas, and a ban by
     score lasted until its next identity.
  2. A gossip op was decoded with the 16 MiB limit of a block, although no op longer than `MAX_OP_BYTES` (64 KiB) can be valid (§4): up to 256 times the longest valid op decoded before the
     Handler refused it.
  3. The gossip messages awaiting the Handler's verdict shared one budget of 256 MiB, so a flood of ops could hold back blocks.
  4. Every inbound sync request spawned a task on the blocking pool at once, each loading up to 500 blocks before the response was cut to its 32 MiB, with up to 100 streams of the protocol per
     connection; nothing bounded those tasks, nor the responses waiting for peers that read slowly. Requests were read up to 32 MiB, although the longest, a `GetBlocks`, takes a few KiB.
  5. A `GetMempool` response could reach the 32 MiB limit (1 000 ops of up to 64 KiB), and a peer's quota allowed 5 of them a second.

  In the RPC servers of the node and the indexer, with jsonrpsee's defaults: a batch could hold any number of calls, a body could take 10 MiB (some 100 000 calls of `ksana_getAccount` in one
  request), and a WebSocket connection 1 024 subscriptions. `ksana_getAccount` cost far more than its answer: it scanned every account's balance in every asset to find the account's own, and
  for every block after the unlock cursor (all the blocks whose fees and rewards wait for their data to land, as many as the settlement backlog holds) `carrier_of` walked the main chain's
  window down from the tip, for the block and again for its carrier, all under the chain's state read lock: the number of those blocks times the length of the walk, for any account.
- **Decision** (all local policy; `ksana-p2p`, `ksana-rpc`, `ksana-node`):
  1. **Sources.** Messages, requests and connections are counted by source: the IP address a peer connected from, an IPv4 address or an IPv6 /64 (an IPv4-mapped address counts as IPv4), or
     the peer itself for a loopback, private, link-local or unspecified address (the node's own machine, its local network, devnet's and the signet's containers) and for an address without IP.
     A peer is counted under the address of its first connection.
  2. **Connections** (`ConnectionLimits`, in `Tuning::connections`): at most 100 inbound connections established, 32 in the handshake, 4 from one address, established and in the handshake
     together (`gate.rs`, before the Noise handshake), and 2 with one peer (libp2p's `connection_limits`). Outbound connections, to the bootstrap peers and explicit dials, are capped only per
     peer. A peer banned by score (3 points, D-040) bans its address for the same 60 seconds: no inbound connection from the address is taken; those already open stay.
  3. **Quotas.** D-059's three token buckets, with their rates and capacities, belong to the source: every identity connecting from one address shares them, and they outlive the connections.
     A source's buckets are dropped once all three have refilled, when they are what a new source gets.
  4. **Gossip.** `/ft/1/ops` takes messages of at most the network's `MAX_OP_BYTES` (`P2pConfig::max_op_bytes`): gossipsub refuses a message longer than that and 4 KiB for its own fields
     (publisher, sequence number, topic, signature, key), the event loop rejects one whose data is longer than `MAX_OP_BYTES` before decoding, at a point of score, and `publish_op` does not
     broadcast one. The messages awaiting the Handler's verdict hold at most 256 MiB for blocks and 32 MiB for ops, a source at most a quarter of each; a message over a budget is Ignored at
     once (not forwarded, no score), and sync fills in later.
  5. **Serving sync.** An inbound request is read up to 64 KiB (`wire::MAX_REQUEST_BYTES`). The Handler serves at most 4 requests at a time, and a source has at most 16 in flight, from intake
     until the response has been sent; the responses ready to be sent hold at most 256 MiB, a source at most a quarter of it, and a response over the budget is dropped (the peer sees a
     failure and asks again). A connection has at most 8 streams of the sync protocol open, both directions together (libp2p's default is 100). A request over these limits is dropped as one
     over the quota is, without score, and the refusals are logged at most every 10 seconds. The node's `GetBlocks` stops loading blocks once their encodings would pass the response's 32 MiB.
  6. **Responses.** A response to `Status` is at most 64 KiB, one to `GetMempool` at most 4 MiB (`wire::response_limit`): the server sends the leading ops that fit, and a client reads a
     response within the limit of the request it answers. A longer one is refused unread, without score while it is within D-040's 32 MiB, which a peer of an earlier version still sends.
  7. **RPC** (`ksana_rpc::server_config`, the same for the node and the indexer): a batch holds at most 64 calls, and a longer one is refused as a whole with jsonrpsee's `-32010` and `id`
     null; a request body, or a WebSocket message, at most 1 MiB, eight times the hex of the longest op (`-32007`, HTTP 413, the body unread); a response at most 10 MiB (`-32008` for a call,
     `-32011` for a batch) and 100 connections at once (HTTP 429 beyond), both jsonrpsee's defaults, now stated; a WebSocket connection at most 16 subscriptions (`-32006`).
  8. **`ksana_getAccount`** reads the account's balance in each asset, one read per asset key (0x03), instead of scanning every balance (0x02), and so does `ksana_getAccountAssets`, which
     lists the same balances first. `ksana_getAccount` finds the carriers of the blocks after the unlock cursor in an index of the main chain built once per call from the cursor's height
     on, extended to the whole window at the first block it lacks, and below the window in the main chain's history on disk, once per block (`btc::Carriers`): `carrier_of`'s answers with
     no landing limit, for one walk of the window. `ksana_getSettlement`, which asks about one block, keeps
     `carrier_of`.
- **Rationale**:
  - Identities are free and addresses are not, so what a source can take is counted by address; an IPv6 /64, because a host is usually given one or more. An honest node needs one connection
    with a peer, two when both dial at once: 4 per address leaves room for a second node behind the same address and a reconnection still in the handshake. The totals are far above what the
    static topology of D-040 needs (the signet runs two seed nodes), and they bound the connections of an attacker who has many addresses.
  - The local addresses are the operator's own, and some deployments hide the remote address behind one (a proxy on the node's machine or its network, Docker's userland proxy): counting
    those by address would make every peer behind them one source, and a ban of one a ban of all. They fall back to the identity and the totals, the protection they had before. Docker's
    published ports keep the remote address of outside peers, so the signet's node1 counts them by address, and node2, which connects over the Compose network, by identity.
  - Quotas that outlive connections close the reconnection bypass at no cost to honest peers, which reconnect seldom. Dropping a source once its buckets are full loses nothing, as a full
    bucket is what a new source starts with, and keeps the memory to the sources that used their quota within a bucket's refill time.
  - The op limit is exact: §4 makes a longer op invalid, so the check refuses no valid op, costs less than decoding one and is a fair reason for a point of score, as no honest peer publishes or
    forwards such an op (`publish_op` refuses it, and so does the validation of every version).
  - A budget per topic keeps a flood of ops from holding back blocks, a share per source keeps one source from filling a topic's budget, and Ignore without score is how D-059 already treats
    traffic over a quota: an honest peer relaying a burst can pass it.
  - Serving 4 requests at a time bounds the blocks being loaded at 4 responses' worth (128 MiB) and the threads busy with them; an honest peer has one request in flight per connection, so 16
    per source leave room for a few nodes behind one address; the response budget bounds what peers that read slowly make the node hold; 8 streams per connection bound the requests being read
    on one connection to 512 KiB.
  - Ops travel by gossip: the `GetMempool` response only fills a new peer's pool, and 4 MiB holds 64 ops of the longest size and thousands of ordinary ones.
  - RPC: the web wallet batches at most 64 calls (`MAX_BATCH`) and sends a batch refused with `-32007`, `-32010` or `-32011` again in halves; no single call comes near 1 MiB (a `ksana_submitOp`
    of the longest op is 128 KiB of hex); a page subscribes to blocks once per connection.
  - `ksana_getAccount`: the answers do not change (the tests compare `Carriers` with `carrier_of` for every block at every cursor height, also below a pruned window), and a call costs one walk of
    the window instead of one per block it asks about.
- **Alternatives**: limits per identity only (a key costs nothing); a proof of work or a fee for connecting (a protocol change for every peer); disconnecting a peer over its quota (honest peers
  relaying a burst would be cut off); refusing sync to peers not yet trusted (a new node must sync from someone); limits per address in the RPC server (jsonrpsee has none, and an operator
  puts the RPC behind a proxy, as the signet does with Caddy); an index of carriers by block in the node's store, written with every main-chain change (a new column family and its upkeep
  for one RPC method).
- **Costs and limitations**:
  - Honest nodes behind one public address (a NAT, a carrier-grade NAT) share 4 inbound connections and one set of quotas: a fifth one dialing in is refused until a place frees up. The quotas
    are far above honest rates.
  - Peers behind a proxy on the node's own network are counted by identity, as before; only the totals bound them.
  - An attacker with many addresses gets a source for each: the totals bound its connections and the work they cause, but it can take the 100 inbound places and keep honest peers that dial
    in out. The node's own outbound connections to its bootstrap peers are not affected, and with static bootstrap (D-040) this is not an eclipse; it stays a limitation until peer discovery
    (§17) chooses peers.
  - A ban by score keeps honest peers sharing the address out for its 60 seconds; their connections already open stay.
  - Gossip dropped over a budget, and responses dropped over theirs, come back by sync and by asking again; a peer of an earlier version whose `GetMempool` response passes 4 MiB fills the
    pool by gossip only.
  - RPC clients must keep batches to 64 calls and bodies to 1 MiB. jsonrpsee's `-32010` and `-32011` for batches share their numbers with Ksana's op errors: they answer a batch, as one
    error object with `id` null, while Ksana's answer a single `ksana_submitOp` with its own `id`.
  - The orphan pool still has no share per source, a note of F-05 that D-098 h listed with F-28: `accept_block` does not know where a block came from, and the pool's caps (256 blocks,
    32 MiB, D-098) bound it in all.
  - Block execution still determines a merged block's carrier with `carrier_of` (`BtcView::carrier`, consensus code), a walk per block bounded by the window; that cost is the block's,
    not a caller's, and is left as it is.
  - The limits by address are tested in unit tests that drive the gate with the swarm's events; the network tests run on loopback, where they do not apply. Nothing was run on a real network.
- **Impact**: `crates/ksana-p2p` (`gate.rs`; `limit.rs`: `IpGroup`, `Source`, the buckets by source and their pruning, `Budget`, `Serving`; `worker.rs`: the connection limits, the bans by
  address, the gossip budgets, the serving; `wire.rs`: `MAX_REQUEST_BYTES`, `MAX_STATUS_RESPONSE_BYTES`, `MAX_MEMPOOL_RESPONSE_BYTES`, `response_limit`, `decode_op` with the op limit, a
  `SyncCodec` that keeps its request's limit; `ConnectionLimits`, `P2pConfig::max_op_bytes`; tests in `tests/codec.rs`, `tests/network.rs` and the unit tests); `crates/ksana-rpc`
  (`server_config` and its constants, `tests/limits.rs`); `crates/ksana-indexer` (`rpc.rs`, a test in `tests/indexer.rs`); `crates/ksana-node` (`btc::Carriers`, `rpc.rs`, `net.rs`, `node.rs`;
  tests in `tests/m23_carrier.rs` and `tests/m25_pruning.rs`); SPEC §11.1, §11.2, §14, §16 and the version history; `docs/RPC.md`; D-040, D-059 and D-098 h (pointing here); the second
  audit's report (F-28, F-29); the site's pages on the RPC, the parameters and security; CLAUDE.md; PROGRESS M49.

## D-133 The TypeScript SDK and the contract playground read bytes as the Rust code does, held to it by a differential fuzz test (2026-10-28, the project owner's request for adversarial testing; the TS SDK, the web page, the playground, the fuzz workspace and the CI's fuzz job; no consensus, node, RPC or contract SDK change, M50)

- **Problem**: the TypeScript SDK mirrors the Rust code that decides what bytes mean: the consensus types and their Borsh codec, the compact
  payloads, BTC transactions and the KSN1 data carriers in them, accounts and addresses, BIP322, the PoW target, the reward schedule and the token
  standards' events. The contract playground's ABI codec mirrors the contract SDK's Borsh decoding, and the testnet points service reads events with
  it (D-126). The shared vectors (`tests/vectors/`) and each side's own tests hold them to the Rust code on the inputs someone wrote down, not on the
  inputs an attacker picks, and a page or a service that reads bytes differently from the node shows, signs or credits what the chain does not have.
  Asked on 2026-10-28 for adversarial testing, with the decisions that follow left to the testing, a differential test ran both sides on the same
  inputs. Of 100 286 requests (the vectors and 100 000 generated, seed 3) the two sides described 19 435 differently, in six ways, every one on the
  TypeScript side:
  1. Medium: **a transaction's data carriers** (1 951 transactions, and all 16 668 requests reading a script or a witness or writing a carrier).
     The SDK knew D-032's KSN1 output but not D-081's envelope in a Taproot witness, and had no envelope functions. Where the node finds a KSN1 output and an envelope
     and takes the transaction for carrying nothing, the SDK took the output's path C op (137); it saw no envelope, well-formed (698, the settlement
     and republish transactions) or malformed (720); and it took a settlement or republish payload from a KSN1 output (396), which the node ignores
     since D-081. A wallet or a service recognizing path C ops with the SDK would take for sent an op that no block will force in. The web page read
     reveal transactions with an envelope parser of its own, which counted neither a KSN1 output beside the envelope nor a malformed envelope, and
     took any payload from it.
  2. Low: **token events whose name starts with U+FEFF** (201). `TextDecoder` drops a leading byte order mark unless told to keep it, and Rust keeps
     it, so the SDK read an event named U+FEFF followed by `transfer` as KS-20's `transfer`, an event the node's standards index (D-120) passes over;
     the web page showed it as one in an operation's details and listed the operation under the accounts it named.
  3. Low: **strings in the playground's ABI codec** (299 events, 54 values): the same byte order mark was dropped from strings and event names. The
     points service, which reads every contract's `standards` event and the events of KS-20 tokens with the codec, would take an event named U+FEFF
     followed by `standards` for a declaration the node's index does not count.
  4. Low: **`Option<Option<T>>` in the playground** (19): its values use `null` for `None`, so `Some(None)` read as `None`, and a form sent `None`
     where `Some(None)` was meant.
  5. Informational: **account text** (115). `parseAccount` and `accountFromAddress` trimmed with JavaScript's `trim`, which removes U+FEFF and keeps
     U+0085, the reverse of Rust's `str::trim`, and `parseAccount` took an upper-case `0X` that `parse_account` refuses. One side accepted text the
     other refused; the two never read different accounts.
  6. Informational: **lists of a zero-sized type in the playground's codec** (128). The contract SDK refuses a `Vec` of a type that takes no memory
     (`()`, a tuple or a struct of such types, `[u8; 0]`, an enum of one variant of such fields, or of none) at any length, as Borsh does; the codec
     refused only `Vec<()>`.

  Agreed on every request: VmOps (their op ID, signing hashes and signature verdicts, signed again by both), payloads, the Borsh encoding of the
  eleven consensus types, BIP322, compact bits and effective targets, Argon2id PoW, the reward schedule, Merkle roots, Taproot addresses and the
  networks' parameters.
- **Decision**:
  1. A differential test in the fuzz workspace, the library `ksana_fuzz::differential` (`fuzz/src/differential/`). A request is one of 18 kinds: a
     VmOp, a payload, a consensus type's Borsh bytes, a BTC transaction, a script, a witness, a payload to write as a carrier, an address, an account,
     a BIP322 signature, compact bits, a header's PoW, the reward schedule at a DAA score, a token standard's event, the Borsh bytes of a contract ABI
     type (45 of them), an event of a contract ABI, Merkle leaves and a network's parameters. The Rust code describes what it makes of the request
     as JSON: refused, or the value in the D-007 JSON form with what is derived from it (hashes, signature verdicts, a re-encoding, a transaction's
     carrier, payload, native op and first input). The Node.js oracle `fuzz/differential/oracle.mjs` describes it the same way with the built SDK
     (`sdk/ts/dist`) and the playground's codec and parser, bundled with the SDK's esbuild, and the two descriptions must be equal. At its start
     the oracle checks that the playground's parser reads the test's ABI types as the test wrote them. Two inputs are left out: a BTC transaction
     without inputs (no block can hold one, and rust-bitcoin reads a segwit-marked one that Bitcoin Core and the SDK refuse) and an event longer
     than a contract can emit (`MAX_EVENT_BYTES`).
  2. It runs as the libFuzzer target `ts_differential` (the request read from the fuzzer's bytes, a divergence a panic printing both descriptions),
     seeded by `fuzz/examples/seeds.rs` with every vector and generated requests, and as the stable sweep `fuzz/examples/differential.rs`: the
     vectors, then requests generated from random values of each kind's structure, from the vectors and the fuzz corpora, some mutated; divergences
     grouped by the kind and the paths at which the descriptions differ, each shown by its smallest input, with a JSON report. The CI's fuzz job
     builds the SDK, runs the sweep of the vectors and 20 000 generated requests, and runs the target for 60 s with the others.
  3. The SDK mirrors `crates/ksana-btc/src/envelope.rs` in `envelope.ts`: `ksn1EnvelopeScript`, `witnessLeaf` (the annex left out, BIP341),
     `isKsn1Envelope` (judged by its start, as a KSN1 output is) and `parseKsn1Envelope`. `txData` counts the KSN1 outputs, then the inputs whose
     witness leaf is an envelope, exactly one in all, and names its `carrier` (`{ kind: 'Output' | 'Input', index }`) where it named the output's
     `vout`; `txPayload` returns `{ carrier, payload }`, and only a payload that fits its carrier, as `tx_payload` does; `nativeOpFromTx` follows.
     `readPush` moves to `btc/script.ts`. The web page reads reveal transactions with `txPayload`, `witnessLeaf` and `parseKsn1Envelope`.
  4. The SDK's event names and the playground codec's strings keep a leading U+FEFF (`ignoreBOM`), as Rust's `from_utf8` does.
  5. The SDK's `parseAccount` and `accountFromAddress` remove the whitespace Rust's `str::trim` removes (the Unicode `White_Space` property), and
     `parseAccount` allows only a lower-case `0x`.
  6. The playground's codec refuses a list of any zero-sized type; its parser refuses an `Option` directly inside an `Option`, saying why (one
     inside a tuple or a struct is fine). The reference for AI assistants and the site's playground page say so.
- **Rationale**: the Rust code is what the chain runs: the node and the indexer recognize transactions and events with it, and contracts decode
  their arguments with the contract SDK. A client that reads the same bytes differently is the one to change, and none of the six is in what the
  node or a contract does. A differential test finds the inputs nobody wrote a vector for (a byte order mark, U+0085, an envelope beside a KSN1
  output) and keeps finding them as either side changes: a new rule on the Rust side that the SDK does not follow fails the CI's fuzz job.
  Comparing whole descriptions, with re-encodings, hashes and verdicts, and not only whether each side accepts, catches the bytes both accept but
  read differently. libFuzzer's coverage guidance reaches deep into the transaction and payload parsers; the stable sweep needs no nightly
  toolchain, starts from every vector and reports each kind of divergence at once with its smallest input. Refusing `Option<Option<T>>` is cheaper
  and safer than a new form of value for a type that no example, standard or reference contract uses: a page that cannot show `Some(None)` must not
  offer the type.
- **Alternatives**: compiling the Rust decoders to WebAssembly for the SDK (one implementation and nothing to compare, but the SDK stays on Web
  standard APIs, D-053, its builders and the web page would still keep types of their own, and the playground's codec is generic over any ABI);
  property tests on each side alone (they hold a side to itself, not to the other); `Some(None)` as a value of its own in the playground
  (`{ some: null }`: a change to the value model the forms, the points service and the reference use, for a type nobody uses); moving the Rust
  side to JavaScript's habits (`0X`, `trim`; the node's RPC and the CLI would change for no gain, and the Rust side is the reference).
- **Costs and limitations**: the test finds differences only: a rule both sides get wrong the same way passes, which the vectors, the spec and the
  audits cover. It covers the formats above and not the SDK's RPC clients and their result parsing, the wallet adapters, PSBT building and signing,
  top-up's lock reading, the web page's own state or the compile service. Every input crosses to Node.js, so the target runs about 2 300 inputs a
  second on this session's VM, and the fuzz job needs Node.js 22 and the SDK built. The SDK's API changes: `TxData`'s `one` names a `carrier` instead
  of a `vout`, and `txPayload` returns `{ carrier, payload }` and `null` for a payload in the wrong carrier (the SDK is not published; the web page
  follows in this change). A contract whose ABI uses `Option<Option<T>>` cannot be used in the playground, and one whose view returns a list of a
  zero-sized type shows an error there.
- **Impact**: `fuzz/` (`src/differential/`, `differential/oracle.mjs`, `fuzz_targets/ts_differential.rs`, `examples/differential.rs`,
  `examples/seeds.rs`, `Cargo.toml` with the library and the crates it compares, which the main workspace already uses, `README.md`); the CI's fuzz
  job; `sdk/ts` (`envelope.ts`, `pathC.ts`, `btc/script.ts`, `btc/address.ts`, `standards.ts`, `index.ts`, the README and tests); the web page's
  `chain/rpc/convert.ts` and a test; the playground's `abi/codec.ts`, `abi/parse.ts`, their tests and the reference for AI assistants; the site's
  security and playground pages in both languages; CLAUDE.md; PROGRESS M50.

## D-134 Mainnet's block share is left out of the published source until it is announced: placeholders in `MAINNET`, a mainnet that does not start, and documents without its values (the project owner, 2026-10-28; revises D-089, D-095, D-096, D-105 and SPEC's preamble, §6.1, §8.6, §13, §13.1, §14, §17; devnet and signet unchanged, no consensus change)

- **Problem**: the repository is published. The project owner, 2026-10-28: "hide only the block share's ratio and the like; the unimportant information already on the
  site does not need hiding". The site already left out the block share's size, how long it lasts and whose account it pays (D-113), while the source and the documents gave
  them: `MAINNET`'s `block_share_bps` and `block_share_end` in Rust and in the TS SDK, the tests of mainnet's issuance, SPEC §8.6, §13, §13.1 and §17, D-089, D-095, D-096 and
  D-105, and the mainnet template's README.
- **Decision**:
  1. `MAINNET` holds placeholders for the block share: `block_share_bps` 0 and `block_share_end` 0 (no share), in `ksana-primitives` and in the TS SDK alike. Every other mainnet
     value stays as it was: the issuance, the PoW limit, the confirmations, the carrier reward.
  2. Mainnet does not start: `GenesisFile::check_network` refuses a mainnet genesis config that passes every other check, saying that the block share is not published, so the
     placeholders can never run a network. The node and the indexer already run the check before they build the genesis block or write any data. The mainnet template and its
     placeholder account stay, as the site describes them; the template's README no longer gives the share's figures.
  3. The documents leave the values out: SPEC §8.6 (the rule without mainnet's values, the carrier's example without the other parts, the emission table's split as "carrier /
     producer and block share", no total of the share), §13 and §13.1 ("not published"), §14 and §17.1; in D-089, D-095, D-096 and D-105 every passage that gave the share's
     size, its length, its total or its recipient leaves it out and is marked [not published, D-134], and a quote of the project owner's that named them is cut there. The tests
     no longer check mainnet's share; those of the rule itself, and of devnet and signet, are unchanged.
  4. devnet and signet keep their block share (2 000 and 50 457 600, D-089): the signet runs with it, and changing it would need a restart from genesis.
  5. Announcing it: the values go into `MAINNET` (Rust and the TS SDK), the documents and the tests, and `check_network` accepts a filled-in mainnet config again. Before mainnet
     launches this is a change of `MAINNET`, not a consensus change (SPEC §17.2).
- **Rationale**: the values are the project owner's to announce, and the site already kept them back. A placeholder that cannot start is safer than leaving a value out of the
  code: a node built from the published source can never run a mainnet with the wrong split, and nobody can take 0 for the decision. The derived figures go too (the per-slot
  parts, the producers' part, the totals), since any one of them gives the fraction back.
- **Costs**: the published documents are less complete until the announcement: the emission tables no longer separate the producers' part from the block share, and a reader of
  DECISIONS sees where something was left out. devnet's 20% stays visible.
- **Impact**: `ksana-primitives` (`MAINNET`, the devnet comment, the tests of the mainnet parameters, of mainnet's issuance and of the carrier); `ksana-execution`
  (`GenesisFile::check_network` and its tests); the TS SDK (`MAINNET`, `params.test.ts`); `mainnet/README.md`; SPEC (the preamble and its version history, §6.1, §8.6, §13, §13.1, §14, §17);
  DECISIONS (the header, D-089, D-095, D-096, D-105, this entry).

## D-135 A header counts on BTC only once its parents have counted: no pending headers, bodies checked when they land, and payloads that carry every header their block needs (the second internal audit's A2-10, 2026-10-29, chosen by the project owner; revises D-077, D-082, D-093, D-095, D-097, D-098, D-100, D-101 and SPEC §6.4, §6.5, §7.1, §7.3, §7.5, §7.6, §10.2, §10.3, §15, §16, §17, `docs/RPC.md`; consensus change, devnet and signet restart from genesis, M51)

- **Problem**: the second internal audit's A2-10. A header in a payload that counts joined the header DAG once its parents had appeared on BTC; until then it was held pending, with no PoW,
  parent-count, version or `bits` check, until the pruning point's anchor passed the header's own anchor, which an attacker sets just below the payload's height. Back-referenced parents cost one
  or two bytes in a payload and 32 bytes plus an index entry in memory, so anyone paying BTC fees could hold roughly 150 to 250 MB on every node and indexer per BTC block of such payloads for about a
  day, for good with `--no-pruning`, written into every checkpoint (the residual of the first audit's F-25 and F-35). The tracker also kept the payloads whose bodies it could not resolve yet, and
  the bodies kept candidates for every header hash without a cap. A body once determined was kept decoded, about 270 bytes for an unlock entry of 3 to 6 bytes in the payload, so one valid header
  with a body of junk entries matching its own `entries_root` held about 7 MB. Bounding the pending set locally means dropping headers when it is full, and an attacker can always fill it: an
  earlier fix that dropped the oldest pending headers was rejected, since those can be valid headers the finality determination waits for.
- **Decision**:
  1. **Counting** (SPEC §6.5 item 1): a header carried by a payload that counts at BTC height h (§7.1) counts at h when its anchor is on the BTC best chain, it passes the header checks (the
     header rules, the anchor not below its parents', the timestamp's anchor bound) and each of its parents has counted at a height at most h, the genesis block always counting. Within one BTC
     block the rule is applied until nothing more counts, so neither the order of the transactions in it nor that of the headers in a payload matters.
  2. **No trace**: an appearance that does not count is ignored. No header is held, no payload is kept unresolved and no candidate body is collected; a header whose parents count later must be
     carried again at or above that height.
  3. **First appearance**: `seen(B)` is the height at which B counts, and its landing (the transaction, its index in the BTC block, the payload's sender) is its earliest appearance in that
     block. Its parents count first, so `conn(B) = seen(B)`, and the finality determination's connection step no longer propagates to children.
  4. **Bodies** (SPEC §6.4, §10.3 item 3): a body lands only with its own header, in a payload at or above the height at which that header counts, when it matches the header's `entries_root`
     once `forced(B)` replaces its `Forced` entries. It is checked when its BTC block is scanned, where `forced(B)` can always be derived (the parents have counted and the anchor is on the
     chain). A block that needs no body lands when its header counts. Landed bodies are kept as the bytes of the transaction that carried them, one shared copy per transaction, and decoded when
     used. The bodies of anchor-invalid blocks (D-056, D-100) are no longer derived for landing: such a block does not count.
  5. **BTC reorgs**: the counted headers and the landings are truncated at the fork and rescanned, as before. The header DAG keeps every header it accepted until pruning (D-097); a header
     accepted while another BTC chain was the best counts on the current one only under item 1 there. Its anchor block being on the current chain, its timestamp bound is the same and is not
     checked again, which settles F-26. §6.5 item 2's exclusion of a block anchored off the chain no longer occurs, since such a block does not count; the check stays.
  6. **Producers** (local policy, SPEC §7.5): the search for headers to carry no longer stops at a block that carries settlement transactions, unless its settlement is in flight (its header
     has not counted and its anchor is at most `IN_FLIGHT_BTC_BLOCKS` = 2 behind the BTC tip) or its header has counted. Otherwise its header is carried, not its body, and its past is
     searched for headers only. Bodies are carried as before.
  7. **Rescue and republishing** (local policy, SPEC §7.3, §7.6): a settlement transaction that confirmed and counted but landed nothing, because a header it needs from outside its payload
     had not counted, is stuck; once those headers count, the payload goes out again unchanged in the order of §7.3 (also when a later payload has counted the block's header meanwhile:
     the body lands only at or above that height), and the block's settlement record stays open until the data it carries has landed (a counted settlement no longer closes it). The
     republish that waited for the next settlement to land without carrying f now waits for it to confirm and count, since that settlement's own data may be waiting for f's headers.
  8. **Checkpoint**: the node's and the indexer's `pruning.ckpt` is format 3, without pending headers, unresolved payloads or candidate bodies.
  9. **Restart**: a chain built under the old rules can hold landings, unlocks and carriers that the new rules place later, so devnet and signet restart from genesis (`signet/README.md`,
     "Restarting from genesis"); mainnet has not started. Signet's PoW limit is raised in the same restart (D-136).
- **Rationale**: whether a header counts is now a function of the current BTC chain alone, not of the order in which a node learned the blocks nor of the BTC chains it saw before, so a running
  node, a restarted one and a fresh indexer agree without any state to carry. Pruning is the one exception, and it decides nothing: a header whose parent has left the header DAG with pruning
  (D-097) does not count, while it counts on a node that has not pruned that far; such a header can be neither on nor merged by a chain through the final blocks (the merge-depth root rule), so
  no outcome depends on it. Nothing waits, so nothing needs a bound: every header in memory paid a valid PoW, and what remains is that PoW's price (below). Honest payloads are closed: a
  producer carries the headers its block needs that have not counted, and a republish run from the first block whose data has not landed holds every parent it needs, so the data landing height
  is unchanged while settlements confirm in order, and can only come later otherwise. The fixpoint within a BTC block lets headers be split across transactions of one block in any order, which
  miners decide.
- **Alternatives**: the audit's suggestion, checking a header before holding it pending (parent count, version, `bits`, PoW at weight 1) and capping the pending set by count and bytes: the check
  costs about 2 Argon2id evaluations per header on signet, and a full cap has to drop headers, valid ones included. Keeping the pending headers on disk instead of in memory (no protocol change):
  memory is bounded, but disk use and rescans still grow with what an attacker pays for, and the checkpoint and restart logic grows. Requiring a parent in an earlier transaction of the block:
  counting would depend on the miner's order. Counting a header whose parents are in the header DAG rather than counted on the current chain: a node that saw another BTC chain would count what a
  fresh one does not.
- **Costs and limitations**: when a header reaches BTC before its parents count, its data lands later, after a rescue: a child's settlement confirming before its parent's (with batching, only
  once the backlog passes the threshold; without it, whenever two settlements confirm out of order; or after a BTC reorg), or a backlog larger than one payload; each costs one more transaction
  pair and about 1 to 2 BTC blocks. A settlement still unconfirmed `IN_FLIGHT_BTC_BLOCKS` after its anchor has its headers carried again by the next payload (14 to about 150 bytes each). A
  republish that leaves out some ancestors lands nothing for the headers depending on them, at its sender's cost. The settled prefix, unlocking, the carrier and BTC finality follow the data
  landing height, so they come later in those cases too. Not addressed (the second audit's A2-07): valid side-branch headers forked from an old block, whose difficulty falls to the PoW limit,
  still count and stay until pruning. With devnet's limit, which signet took until now, that was about 2 Argon2id evaluations per header of a non-empty block, about 85 000 such headers (about
  85 MB) per full BTC block, or about 250 000 headers of empty blocks (about 250 MB) at 12 to 120 times the work; on mainnet at least about 65 536 evaluations per header. The same headers can
  land junk bodies, about 4 MB of transaction bytes per full BTC block. D-136 raises signet's PoW limit against it; not counting side headers below the finalized tip beyond the merge depth
  (below the finalized tip alone would split nodes, since a block may still merge headers below it but above the merge-depth root) is a separate design, not adopted. Scanning a BTC block costs
  about the bytes of its payloads: the leaf hash each native transaction adds to `entries_root` as a forced-inclusion entry is computed once, when its BTC block is scanned, and kept with it
  (32 bytes); whether a counted block needs a body is decided from the leaf hashes of its window, one hash per native transaction for each distinct window among the headers counting in that
  BTC block; and a carried body is checked from its own entries and those leaf hashes. No native op is copied or encoded again, however many headers or bodies name it, so neither the time nor
  the memory of a scan grows with the size of the ops a window holds. The restart clears balances and contracts on signet.
- **Impact**: `crates/ksana-btc` (`tracker.rs`: the scan by BTC block, `KnownBlocks`, `unresolved` and `resolve*` gone, the native transactions kept with their leaf hashes; `headers.rs`:
  `Pending` gone, the counting rule; `finality.rs`: `connect`; `bodies.rs`: candidates and the anchor-invalid derivation gone, bodies checked at the scan from leaf hashes (`converted_root`)
  and kept encoded; `native.rs`: `forced_leaf`; `prune.rs` and the checkpoint records); `crates/ksana-crypto` (`merkle.rs`: `leaf_hash`, `entry_leaf`, `root_of_leaf_hashes`, the same roots);
  `crates/ksana-node` (`chain.rs`: `KnownBlocks`, `payload_carry`, `process_settlements`, `CHECKPOINT_VERSION` 3; `rescue.rs`: the stuck settlement and the republish condition; `btc.rs`; the
  memory profile's snapshot); `crates/ksana-indexer` (`known.rs`, the checkpoint); tests (a property test of the tracker, adversarial payloads, regtest scenarios); SPEC (the preamble and its
  version history, §6.4, §6.5, §7.1, §7.3, §7.5, §7.6, §10.2, §10.3, §14, §15, §16, §17.5); ARCHITECTURE; RPC (`ksana_getSettlement`'s `confirmed`); SIGNET; the second audit's report (A2-07,
  A2-10, A2-15, A2-19, F-03, F-25, F-26, F-35); the site's pages on how it works, security, the glossary, the RPC and confirmations; `signet/genesis.json` after the restart.

## D-136 Signet's PoW limit is raised to `0x1f0fffff`, about 4 096 Argon2id evaluations per block at the limit (the part of the second internal audit's A2-07 that D-135 leaves, 2026-10-29, chosen by the project owner with D-135; revises D-055 and SPEC §6.6, §13, §13.1, §16, §17.5; a consensus change for signet only, at the restart of D-135, M51)

- **Problem**: D-135 holds nothing for a header that does not count, but a valid header counts and stays until pruning (D-097). A side branch forked from an old block has its difficulty fall as
  its timestamps run ahead of its blocks (ASERT, SPEC §6.7), down to the PoW limit, and signet took devnet's limit `0x207fffff`, a target of about 2^255: a header there takes about 2 Argon2id
  evaluations. A full BTC block of such headers, about 85 000 headers of non-empty blocks (46 bytes each in a payload) holding about 85 MB on every node and indexer, cost about 3 CPU-minutes;
  about 250 000 headers of empty blocks (14 bytes each, about 250 MB) cost 12 to 120 times that work, about 1.5 to 14 CPU-hours.
- **Decision**: `SIGNET.pow_limit_bits` is `0x1f0fffff` in `ksana-primitives` and in the TS SDK, a target of about 2^244: at the limit a block takes about 4 096 evaluations, about 3.5 seconds
  of one core at D-094's 0.86 ms per solve attempt. A full BTC block of side-branch headers then costs about 3.5 CPU-days for headers of non-empty blocks and at least about 120 CPU-days for
  headers of empty blocks. Signet's genesis `bits` stays as `signet/ksana-signet.sh` sets it, `0x1f0d1b71` for one seed node and `0x1f068db8` for two (about 5 000 and 10 000 evaluations),
  both harder than the new limit. devnet keeps `0x207fffff`, which lets it produce blocks every 5 seconds on any CPU from genesis, and mainnet keeps `0x1f00ffff` (about 65 536 evaluations per
  header, about 55 CPU-days per full BTC block).
- **Rationale**: signet's difficulty has stayed between about 10 000 and 16 000 evaluations per block (about 14 300 on 2026-10-29, `bits` `0x1f04913a`), 2.4 to 3.9 times above the new limit,
  so honest producers never mine at it; one seed node on one thread solves a block at the limit in about 3.5 seconds, under the 5-second block time, so a signet left with a single producer
  still settles above the limit. The limit is the cheapest lever on A2-07: it raises the price of every side-branch header about 2 000 times, in the same restart as D-135, without a new rule, and the dead
  tips relayed over P2P that A2-07 first described pay the same price.
- **Alternatives**: mainnet's `0x1f00ffff`: about 65 536 evaluations, above signet's own difficulty, so its two seed nodes would mine at the limit, about one block per 28 seconds instead of 5.
  A limit close to signet's current difficulty (about 16 000): too little room for ASERT to lower the difficulty when a producer stops. Not counting side headers below the finalized tip beyond
  the merge depth: a rule of its own, not adopted (D-135, costs).
- **Costs**: tools that took signet's parameters to be devnet's but for the chain ID and the anchor depth must take the new limit; the TS SDK carries it, and the web wallet, which pins this
  repository, takes it with its submodule. A side-branch header at the limit costs anyone about 2 000 times more CPU, honest producers nothing.
- **Impact**: `ksana-primitives` (`SIGNET` and its tests), the TS SDK (`SIGNET`, `params.test.ts`); the PoW vectors' signet case, whose header has devnet's `bits`, keeps its values
  and is judged against devnet's limit (`gen_pow_vectors`, the case's note in `tests/vectors/pow.json`); SPEC (the preamble and its version history, §6.6, §13, §13.1, §16, §17.5);
  SIGNET; the site's pages on the parameters, the signet and mining; ksana-web's submodule.
