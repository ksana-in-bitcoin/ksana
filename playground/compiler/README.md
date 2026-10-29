# The playground's compile service

Builds a contract's source as one of the playground's example crates and answers with the module and rustc's diagnostics
(D-119). The hosted signet runs it at `https://signet.ksana.network/compile`, in a container of its own behind Caddy
(`signet/docker-compose.yml`, the `compiler` service), and the playground's page at `playground.ksana.network` calls it from
its own origin, which the service allows (`KSANA_COMPILER_CORS`). The page's client is `playground/src/compile/client.ts`.

## API

`POST /` (or `/compile`, the path Caddy gives it) with `content-type: application/json`:

```json
{ "source": "<the whole src/lib.rs>", "crate": "amm" }
```

`crate` is one of `crates.json`. The source replaces that crate's `src/lib.rs` for the build, which is the one
`playground/scripts/build-examples.mjs` runs:
`cargo rustc --locked --offline --release --target wasm32-unknown-unknown -p <crate> --crate-type cdylib`, with the contracts
workspace's linker flags and the source paths remapped, and nothing in it depends on where the slot is (the slot's workspace
reaches the SDK through `contracts/sdk`, a link inside its root, as cargo would otherwise hash the SDK's absolute path into every
symbol name, D-031) or on the machine's architecture (cargo runs rustc through `lib/rustc.sh`, which names one host on every
machine, as cargo hashes the host into the same names). An example's own source therefore gives the module the playground
ships, byte for byte. The answer is
`200` with either

```json
{ "ok": true, "wasm": "<base64>", "sha256": "<hex of the module>", "diagnostics": [], "ms": 4210 }
{ "ok": false, "diagnostics": [{ "level": "error", "message": "...", "rendered": "...", "spans": [] }], "ms": 980 }
```

Diagnostics are rustc's errors and warnings about the source: `spans` hold their positions in it (`line_start`, `line_end`,
`column_start`, `column_end`, `is_primary`, `label`), and `rendered` is the text a terminal shows, with paths written as
`src/lib.rs`, `/ksana/` and `/cargo`. A build stopped by its time limit, or one cargo itself refuses, is `ok: false` with a
diagnostic of the service's own and no span.

`GET /health` (or `/compile/health`) answers `{ "ok": true, "rustc": "rustc 1.98.1 (...)", "crates": [...], "slots": 1,
"busy": 0, "queued": 0 }`.

Errors are `{ "error": "<reason>" }` with `400` (no source, an unknown crate, a body that is not JSON), `413` (a source over
128 KiB), `415` (not JSON), `429` (too many builds from this address, with `retry-after`), `503` (the queue is full, with
`retry-after`), `404` and `500`.

## Limits

- A source of at most 128 KiB; a module of at most 1 MiB is sent back (an operation carries at most 64 KiB, so a module that
  deploys is far smaller).
- One build at a time per slot (one slot by default) and at most 8 waiting for one; a request that leaves while it waits, or
  while its build runs, frees its place and stops the build.
- A build is stopped after 120 seconds, with rustc and the linker (each build runs in a process group of its own).
- 6 builds a minute from one address, as many at once; behind Caddy the address is the first of `X-Forwarded-For`, which Caddy
  sets afresh for every request (`KSANA_COMPILER_TRUST_PROXY=1`).
- At most 200 diagnostics, each rendered text at most 16 KiB.

## What a build can reach

A build runs what a source asks of the compiler: `include_bytes!` and `include_str!` read files, `env!` reads the build's
variables, and const evaluation computes. Nothing else can run: the visitor gives only `src/lib.rs`, so there is no build script
or procedural macro but those of the crates' fixed dependencies, and the build is offline. So the service is built to hold
nothing worth reading:

- A slot is a copy of the parts of the repository a build reads (the root manifest and `rust-toolchain.toml`, the SDK's
  manifest and sources, and `contracts/` with its lockfile and linker flags, its manifest naming the SDK through the link
  `contracts/sdk`), and the example's own source is put back after every build.
- A build gets only the variables cargo and rustup need (`PATH`, `HOME` set to the slot, `CARGO_HOME`, `RUSTUP_HOME`, the target
  directory and the flags); the service's own environment stays out.
- The hosted container holds no secret, has no capabilities, at most 512 processes, 2 GB of memory and one core at a low CPU
  weight (the signet's seed nodes mine first), a `tmpfs` for `/tmp`, and a network that only Caddy shares and that leads
  nowhere else. Its image fetches the crates and builds every example once in its slot when it is built
  (`node server.mjs --prepare`), so builds afterwards need no network.

## Running it

It needs Node.js 20.19 or later and the toolchain `rust-toolchain.toml` names, with its `wasm32-unknown-unknown` target
(`rustup toolchain install` in the repository's root installs both). It runs on Linux and macOS; on macOS `lib/rustc.sh` gives
rustc the toolchain's `lib` directory in `DYLD_FALLBACK_LIBRARY_PATH`, as rustup's proxy does, because the toolchain's
rust-lld finds `libLLVM.dylib` only there and macOS drops the variable when it starts the script's `/bin/sh`:

```bash
node playground/compiler/server.mjs --prepare   # make the slots and build every example once in each, then exit
node playground/compiler/server.mjs             # serve on 127.0.0.1:8090 (the slots are made first when missing)
KSANA_RPC=http://127.0.0.1:18545 KSANA_COMPILE=http://127.0.0.1:8090 npm --prefix playground run dev
```

Settings, in the environment:

| Variable | Default | Meaning |
| --- | --- | --- |
| `KSANA_COMPILER_HOST`, `KSANA_COMPILER_PORT` | `127.0.0.1`, `8090` | where it listens |
| `KSANA_COMPILER_REPO` | the repository holding this file | what the slots are made from |
| `KSANA_COMPILER_SLOTS_DIR` | `$TMPDIR/ksana-compiler` | where the slots live |
| `KSANA_COMPILER_SLOTS` | `1` | builds at once, a slot each |
| `KSANA_COMPILER_QUEUE` | `8` | builds that may wait for a slot |
| `KSANA_COMPILER_TIMEOUT_SECS` | `120` | when a build is stopped |
| `KSANA_COMPILER_PER_MINUTE` | `6` | builds a minute per address, `0` for no limit |
| `KSANA_COMPILER_TRUST_PROXY` | unset | `1`: the address from `X-Forwarded-For` |
| `KSANA_COMPILER_CORS` | unset | the origin of a page that calls it from another origin (the hosted signet's: `https://playground.ksana.network`) |
| `KSANA_COMPILER_SHIPPED` | unset | with `--prepare`: a `builds.json` every example's build must equal |

A slot is kept while the repository's files it was made from are unchanged and its own are as it made them, and made again
when the service starts otherwise, after a change to the contracts workspace or the SDK for instance (the hosted image makes its
slots when it is built, and checks there that every example's own source gives the module the playground ships,
`KSANA_COMPILER_SHIPPED`). `npm test` in `playground/` runs
`test/build.test.mjs`, which builds every example in a slot of its own and compares it with the shipped module (skipped with
`KSANA_COMPILER_SKIP_CARGO=1`), and `test/service.test.mjs`, the HTTP side with a stand-in builder. The builds are offline, as
the service's are, so the contracts workspace's crates must be in the cargo home first:
`cargo fetch --locked --manifest-path contracts/Cargo.toml` from the repository's root, as CI does.
