# Ksana contract playground

A page of its own, hosted at [playground.ksana.network](https://playground.ksana.network/) on the public signet (D-119): eight
example contracts, each with a page made for it, its Rust source to edit and compile in the browser, a form for every method
and the operations this browser sent, and a reference for AI assistants that write contracts (`#/ai`). Every operation is
signed in the visitor's own wallet (Unisat, OneKey or Xverse, a Taproot account on Bitcoin signet) and pays its fee in test
KSA. The guide for users is the site's
[Contract playground](https://ksana.network/docs/playground/) page (`content/<language>/docs/playground.md` in the
[ksana-site](https://github.com/ksana-in-bitcoin/ksana-site) repository).

The playground runs on the signet and, for development, a local devnet; it refuses any other network. Its text is in English
and Simplified Chinese (`src/i18n/en/`, the source language whose structure is the `Messages` type, and `src/i18n/zh-CN/`,
the only Chinese in this directory). Like the web wallet it opens in English unless `?lang=zh-CN` or the choice made in its
settings says otherwise (D-115).

## Running

Node.js 20.19 or later (22 in development). The playground uses the repository's TypeScript SDK through `file:../sdk/ts`, so
build the SDK first:

```bash
(cd sdk/ts && npm ci && npm run build)
cd playground
npm ci
npm run dev                  # http://localhost:5174
npm test                     # vitest (the ABI parser and codec, the journal, the examples' math and rules, the AI reference), then the compile service's tests
npm run typecheck
npm run build                # dist/, a static site
npm run build:hosted         # dist/ for playground.ksana.network: builds the SDK first, adds hosted/deployment.json and _headers
```

`npm run dev` talks to whatever its address bar names. Against a devnet (`docker compose -f devnet/docker-compose.yml
--profile nodes up -d --build`) with a compile service of your own:

```bash
KSANA_RPC=http://127.0.0.1:18545 KSANA_COMPILE=http://127.0.0.1:8090 npm run dev
# then open http://localhost:5174/?rpc=/rpc&compile=/compile
```

The development server proxies `/rpc`, `/compile` and, with `KSANA_FAUCET`, `/faucet` to those addresses. On a devnet the
wallet can be a test key in the address bar, `?wallet=mock&key=<64 hex digits>` (read once and removed from the address; no
other network uses it). Without a compile service the examples' published modules still deploy; edits then need a module
built locally and uploaded on the Code tab. The signet's rehearsal (`KSANA_REHEARSAL_KEEP=1 signet/rehearse.sh`) allows the
development server's origin as the hosted signet allows the hosted page's, so
`http://localhost:5174/?rpc=http://127.0.0.1:18080/rpc&faucet=http://127.0.0.1:18080/faucet&compile=http://127.0.0.1:18080/compile`
uses its whole stack across origins, as the hosted page does.

## Where it finds the chain

A hosted copy has `deployment.json` next to `index.html`:

```json
{ "chain": "signet", "rpc": "https://signet.ksana.network/rpc", "faucet": "https://signet.ksana.network/faucet",
  "compile": "https://signet.ksana.network/compile", "explorer": "https://signet.ksana.network/",
  "genesis": "<genesis block hash>", "instances": { "amm": "<contract ID>" } }
```

`instances` names contracts the operator deployed for everyone to use: an example's page lists its shared contract first and opens
it for a visitor who has none of their own. They are offered only on the chain whose genesis block is `genesis` (the hash
`ksana_getBlock` gives for height 0), so a network started again from genesis, which keeps its chain ID, offers none of the
contracts before it until the file names the new chain's; without `genesis` none are offered. What the page keeps in this
browser for a chain (the contracts deployed or opened, the journal of operations) is kept under its chain ID and genesis block
for the same reason. A copy without the file takes `?rpc=`, `?compile=`, `?faucet=` and `?explorer=` from the address bar. The page's settings can change each endpoint;
what they hold is kept in this browser and wins over both. The copy at playground.ksana.network has `hosted/deployment.json`,
which `npm run build:hosted` puts next to `index.html` (`src/chain/deployment.test.ts` checks that the page reads it), so a
shared contract is named there and deployed with the page. `version.json` beside it names the commit the page was built from.

## The examples

| Example | Crate (`contracts/examples/`) | Page (`src/examples/`) |
| --- | --- | --- |
| Counter | `playground-counter` | `counter/` |
| Asset | `mintable-token` | `token/` |
| KS-20 token | `token` | `ks20/` |
| NFT (KS-721) | `nft` | `nft/` |
| AMM | `amm` | `amm/` |
| Lending | `lending` | `lending/` |
| Multisig | `multisig` | `multisig/` |
| Vesting | `vesting` | `vesting/` |

Each example is a crate of the contracts workspace with its native tests, its module in `src/examples/wasm/<crate>.wasm`, a
page, and its messages in `src/i18n/<language>/<example>.ts`. The page reads the crate's `src/lib.rs` at build time, shows it
on the Code tab, and takes the generic forms and the decoding of results and events from its ABI block.

The modules are built reproducibly: `npm run build:examples -- [<crate> ...]` builds them as the compile service does, in a
slot of its own (`contracts/target/playground-slot`), with the pinned toolchain (`rust-toolchain.toml`, with the wasm32
target), the contracts workspace's linker flags and the paths remapped, and writes `src/examples/wasm/builds.json` with the
SHA-256 of each source and module. A slot is a copy of the contracts workspace whose manifest reaches the SDK through
`contracts/sdk`, a link inside its root: cargo hashes a path dependency outside the workspace's root by its absolute path, and
the symbol names, and so the modules, would follow where the repository is (D-031). Cargo hashes the machine's host into the
same names, so every build runs rustc through `compiler/lib/rustc.sh`, which names one host on every machine. So an example's
own source compiles to the very module the page ships, byte for byte, wherever and on whatever machine it is built;
`src/examples/examples.test.ts` fails while a source differs from the one its module was built from, the compile service's
`test/build.test.mjs` builds every example in another slot and compares, amm and multisig again with a rustc that answers as a
machine of another architecture, and `crates/ksana-vm/tests/playground.rs` runs the shipped modules through the VM's
deployment checks.

To add an example:

1. Write the crate in `contracts/examples/<crate>/` with an ABI block and native tests, and add it to the contracts workspace.
2. Name it in `compiler/crates.json` (the compile service builds only these) and run `npm run build:examples -- <crate>`.
3. Register it in `src/examples/index.ts` (its source, module, page and `init` presets) and add the module to the VM test.
4. Write its page under `src/examples/<example>/` and its messages in both languages.
5. Add its row to the table of pages in the AI reference's "The playground" (`src/ai/ksana-contracts.md`; the tests fail
   until it is there).
6. List it in the site's docs (`content/<language>/docs/playground.md` and `example-contracts.md` in ksana-site).

## The ABI block

A contract describes its methods in a fenced block marked `abi` in the `//!` documentation at the top of its `src/lib.rs`:

```rust
//! ```abi
//! init()                          // starts the count at 0
//! call increment() -> u64         // adds one; returns the new count
//! call add(amount: u64) -> u64    // adds 1 to 1000 at a time; returns the new count
//! view get() -> u64               // the count
//! event changed(by: Account, value: u64)
//! ```
```

`init`, `call` (an operation; `payable` before it takes attached funds), `view` (read-only), `event`, `struct` and `enum`, one
per line. The types are the integers, `bool`, `String`, `Bytes`, `[u8; N]`, `Account`, `ContractId`, `AssetId`, `Hash`,
`Option<T>`, `Vec<T>`, tuples, `()` and the declared structs and enums, all Borsh-encoded as the contract encodes them. The
grammar is in `src/abi/parse.ts`, the encoding in `src/abi/codec.ts`; the site's page describes it for users.

## Operations

Every write goes the same way (`src/chain/run.ts`): a read-only run of the call against the latest state (its gas, and whether it
would fail now), a review the person signs from, then the submission through the journal of `src/chain/ops.ts` (D-118, moved
here from the web page's first playground with D-119). The journal is written to `localStorage` before the operation is sent, so
a lost answer never leads to a second operation at the same nonce: the nonce stays reserved until the node shows the operation,
or the person releases it after the node reports it unknown. Writes for one chain and account take a Web Lock, so two tabs never
sign at the same nonce. No key, signature, argument or code is stored.

## AI assistants

`src/ai/ksana-contracts.md` is a reference for AI assistants that write, review or fix contracts (D-124): in English, in one
Markdown file, it gives how a contract runs, every public item of the contract SDK as a Rust signature, gas and limits, the
built-in assets, KS-20 and KS-721, the ABI block, this playground, and complete contracts. The page `#/ai`
(`src/ai/AiPage.tsx`) copies it to the clipboard in one press, or downloads it, with prompts to start from; the Code tab's
**Copy for an AI assistant** copies it followed by the source in the editor and the compiler's last messages about that
source, with what the assistant should do (`codeContext` in `src/ai/reference.ts`), and **Only the code and errors**
the same without it. The text is bundled with the page, since a browser copies to the clipboard only within the press. The
site publishes the same file as `https://ksana.network/llms-contracts.txt`.

Tests hold the reference to the code, so a change that it no longer describes fails until it does:

- `crates/ksana-contract-sdk/tests/reference.rs`: the SDK's signatures, both ways (every public item is listed, as the
  source declares it, and nothing else), and the complete contracts, which are the repository's own files, built and run:
  `contracts/examples/poll` with one of its tests, and a KS-20 token and an asset under `tests/reference/`.
- `crates/ksana-execution/tests/reference.rs`: the gas and limits tables and the networks' parameters, built from the
  constants.
- `src/ai/reference.test.ts`: this playground as the reference describes it (each page with its crate and features, the
  compile service's limits, the faucet), the ABI block's types and every ABI block in it parsed, and what the Code tab
  copies.

## Directory layout

```
playground/
  compiler/          the compile service (README.md)
  hosted/            deployment.json and _headers of the copy at playground.ksana.network
  scripts/           build-examples.mjs
  public/            the icons
  src/
    abi/             the ABI block's parser, types, codec and text forms
    ai/              the reference for AI assistants, its page (#/ai) and what the Code tab copies
    app/             the frame, routes, stores and preferences, the workspace (edited sources and builds)
    chain/           the node, the wallet, the account, assets, views, the journal and operations
    compile/         the compile service's client
    editor/          the source editor (CodeMirror 6)
    examples/        the examples' registry, their pages, the shared kit, and wasm/ with the shipped modules
    i18n/            en/ and zh-CN/
    styles/          tokens, base and the app's styles
    ui/              the shared panels: Code, Methods, Activity, Deploy, the review, values and fields
```

## Hosting

A Cloudflare Worker serves the page at playground.ksana.network (`wrangler.jsonc`: static files only, no Worker script, as for
the project's site). Workers Builds, with `playground` as the root directory, runs `npm run build:hosted` and then
`npx wrangler deploy` at every push to `main` that changes `playground/`, `sdk/ts/` or `contracts/examples/` (its build watch
paths); `npx wrangler deploy` in this directory does the same from a machine. The page calls the signet's server across
origins: node1's JSON-RPC, which allows any origin, and the faucet and the compile service, which allow this page's alone. The
compile service runs there, in the Compose project's `playground` profile (`signet/Dockerfile`'s `compiler` stage):
`signet/README.md` ("The contract playground") is the operator's guide. A change to an example, the SDK or the compile service
reaches the page with the next push to `main`, and the compile service with the server's next deploy.
