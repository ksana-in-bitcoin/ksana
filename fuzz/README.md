# Fuzz tests (SPEC §15, D-050)

cargo-fuzz (libFuzzer) targets, separate from the main workspace (nightly required):

| Target | Input | Property |
| --- | --- | --- |
| `vmop_decode` | arbitrary bytes as a VmOp encoding | no crash; when decoding succeeds the encoding is canonical, the signature message is a prefix of the full encoding, and signature verification can only fail, never crash |
| `payload_decode` | arbitrary bytes as a payload encoding, a KSN1 data script, a payload placed in a transaction | no crash; canonical encoding; the data script round-trips through rewriting; `tx_payload` agrees with direct decoding |
| `btc_inputs` | settlement transactions | decoding, payload, native transaction recognition, funding inputs and settlement validation (`validate_attached`) can only return errors (path A's PSBT was removed together with path A, D-078) |
| `host_params` | the test contract calls host functions one after another as directed by the input, with all parameters taken from the input | `vm.call` does not return `VmError`; two executions of the same input give the same result and host state; gas does not exceed the limit |
| `bip322_witness` | the first byte selects the account (valid account / off-curve / all zeros), the rest is a BIP322 simple witness encoding | `verify_for_account` and `verify_vmop` can only return errors, never crash |
| `ts_differential` | a request of one of 18 kinds read from the input (`Request::from_fuzz`): a VmOp, a payload, a consensus type's Borsh bytes, a BTC transaction, a script, a witness, a payload to write, an address, an account, a BIP322 signature, compact bits, a header's PoW, the reward schedule, a token standard's event, Borsh bytes of a contract ABI type, an event of a contract ABI, Merkle leaves, a network's parameters | the TypeScript SDK and the contract playground's ABI codec describe the input exactly as the Rust code does (D-133, below) |

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz
cargo run --manifest-path fuzz/Cargo.toml --example seeds   # generate the seed corpus fuzz/corpus/<target>/
cd fuzz && cargo +nightly fuzz run host_params -- -max_total_time=60
```

The CI `fuzz` job runs each target for 60 seconds starting from the seed corpus; when a crash is found, the triggering
input is uploaded as an artifact (reproduce locally with `cargo +nightly fuzz run <target> <file>`). `corpus/` and
`artifacts/` are not checked in.

## Differential test of the TypeScript implementations (D-133)

The TypeScript SDK (`sdk/ts`) mirrors the Rust code that decides what bytes mean, and the contract playground's ABI codec
(`playground/src/abi/codec.ts`, used by the testnet points service too) mirrors the contract SDK's Borsh decoding. The
library `ksana_fuzz::differential` (`src/differential/`) sends each request to the Rust implementation and, through the
Node.js oracle `differential/oracle.mjs`, to the TypeScript one; each describes what it made of the input as JSON (refused,
or the value in the D-007 JSON form with what is derived from it: hashes, signature verdicts, a transaction's carrier,
payload and native op), and the two descriptions must be equal. The oracle loads the built SDK (`sdk/ts/dist`) and bundles
the playground's codec and parser with the SDK's esbuild; at its start it checks that the playground's parser reads the
test's ABI types (`src/differential/abi.rs`) as the test wrote them.

It needs Node.js 22 and the SDK built:

```sh
(cd sdk/ts && npm ci && npm run build)
# the libFuzzer target, from the seed corpus (examples/seeds.rs writes the shared vectors and generated requests into it)
(cd fuzz && cargo +nightly fuzz run ts_differential -- -max_total_time=600)
# the stable sweep: the shared vectors as they are, then generated and mutated requests spread over the kinds; divergences
# grouped by kind and the paths that differ, each shown by its smallest input; exit status 1 on a divergence, 2 when the
# oracle fails
cargo run --release --manifest-path fuzz/Cargo.toml --example differential -- --cases 100000 --seed 3 --out report.json
cargo run --release --manifest-path fuzz/Cargo.toml --example differential -- --kind tx --kind script --no-vectors
```

`KSANA_DIFF_SDK=<path to dist/src/index.js>` runs the oracle on another build of the SDK, an older or a patched one, and
`KSANA_DIFF_NODE` names the Node.js program (default `node`). A divergence that libFuzzer finds is a panic printing the
request and both descriptions, and the input is kept in `artifacts/ts_differential/`. Left out on purpose: a BTC
transaction without inputs (no block can hold one, and rust-bitcoin reads a segwit-marked one that Bitcoin Core and the
SDK refuse) and an event longer than a contract can emit (`MAX_EVENT_BYTES`).
