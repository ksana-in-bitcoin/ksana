# Ksana smart contracts: a reference for AI assistants

Give this file to an AI assistant (ChatGPT, Claude, Gemini, Copilot or any other) before you ask it to write, explain,
review or fix a Ksana contract. It holds what the assistant needs so that it guesses nothing: how a contract runs, every
function, type and constant of the contract SDK, gas and limits, the token standards, the playground's ABI block, how to
test, build and deploy, and complete contracts. The repository's tests hold it to the code: its SDK signatures, its gas
and limits tables and its examples are checked against the source.

Ksana is a smart-contract layer on Bitcoin. Contracts are WebAssembly modules written in Rust; Bitcoin orders and
settles the blocks; the native token is KSA. The public test network is the signet:

- Playground, to write, compile, deploy and call contracts in the browser: https://playground.ksana.network/
- Web wallet and explorer: https://signet.ksana.network/
- Node JSON-RPC: https://signet.ksana.network/rpc
- Docs: https://ksana.network/docs/ (writing contracts: https://ksana.network/docs/contracts/, the SDK:
  https://ksana.network/docs/contract-sdk/, a first contract in the playground: https://ksana.network/docs/first-contract/)
- This file: https://ksana.network/llms-contracts.txt
- Source: https://github.com/ksana-in-bitcoin/Ksana (the SDK in `crates/ksana-contract-sdk`, the VM interface in
  `docs/ABI.md`, the token standards in `docs/STANDARDS.md`, the example contracts in `contracts/examples`)

## Rules for the assistant

1. Write one file, `src/lib.rs`, in Rust (edition 2021, Rust 1.98.1) for the target `wasm32-unknown-unknown`, using
   only `core`, `alloc` and the crate `ksana_contract_sdk` as this file lists it: no `std` on wasm32 and no other
   crate. Ksana is not Ethereum, NEAR, Solana, CosmWasm or ink!, and none of their APIs, macros or attributes exist.
   `msg.sender` is `sdk::caller()`; attached funds (`sdk::call_funds()`) take the place of `msg.value` and
   `payable`; `init` is the constructor; `if !condition { return sdk::revert("reason"); }` takes the place of
   `require`; a mapping is storage keys under a prefix.
2. The skeleton is fixed: `#![cfg_attr(target_arch = "wasm32", no_std)]`, `extern crate alloc;`,
   `sdk::entry!(dispatch);` and `pub fn dispatch(method: &str, args: &[u8]) -> CallResult`, which matches on the method
   name. Deployment always calls `"init"`, so `dispatch` answers it even when it takes no arguments (an `unknown method`
   there fails the deployment); any other name it does not know is `sdk::revert("unknown method")`.
3. Arguments and return values are Borsh. Decode with `sdk::decode::<T>(args)?` (a tuple for several arguments, `()`
   for none; every byte must be used) and answer with `sdk::ok(&value)`. A revert undoes everything the operation did,
   at every level, and its reason is what the user sees: keep it short, lowercase and specific.
4. Never panic: no `unwrap`, `expect`, `panic!`, `[]` indexing or unchecked arithmetic. Use `checked_add`,
   `checked_sub`, `checked_mul`, `get`, `get_mut`, `usize::try_from` and `?`. A panic is a trap: the operation fails
   without a reason the user can read.
5. Integers only: an `f32` or `f64` anywhere in the module gets the deployment refused. Amounts are `u128` in base
   units. There is no randomness, and the only clock is `sdk::block_timestamp()`, in seconds and only roughly right.
6. Nothing in memory survives a call: each call runs in a new instance, and what must last goes into `storage`.
   Storage cannot be listed or iterated, so keep counts and index keys yourself, such as
   `[b"item:".as_slice(), &id.to_be_bytes()].concat()`. A write is the dearest thing a contract does (see Gas): store
   little, and remove a key whose value goes back to nothing.
7. A method that takes no attached funds refuses them first,
   `if !sdk::call_funds().is_empty() { return sdk::revert("unexpected attached funds"); }`, or they stay in the
   contract for good. A method that takes them checks which assets and amounts arrived.
8. Write storage before calling another contract or paying out (checks, effects, interactions): calls can re-enter
   the contract.
9. Put an ABI block in the `//!` documentation at the top of the file (see The ABI block) and keep it in step with
   `dispatch`, so that the playground builds a form for every method and decodes results and events.
10. Emit an event for each change others follow: `sdk::emit(&("name", field, field))`.
11. Use only the functions, types and constants listed under The contract SDK. Native tests use
    `ksana_contract_sdk::mock`.
12. Give the whole `src/lib.rs`, ready to paste into the playground's Code tab, and name the page that builds it: Counter
    for a contract on the plain SDK, or KS-20 token, NFT or AMM for one that uses `ks20`, `ks721` or `standard`.
13. Where the request leaves a choice open, such as who may call a method, whether an amount may be zero or what happens
    at a limit, make the safer choice and say in a sentence what you chose.

## How a contract runs

- **Contracts and accounts.** A contract is a WebAssembly module with its own storage and its own balances of built-in
  assets, under a 32-byte contract ID. A user's account is 32 bytes too: the output key of their Taproot address, the
  witness program of its `bc1p…` (mainnet) or `tb1p…` (signet) address. Both kinds of account hold assets the same way.
  The zero account, 32 zero bytes, is nobody.
- **Operations.** Everything happens through operations: one signed message from one account that deploys a contract,
  calls one or transfers an asset, with a `max_gas` and a fee. Path B sends it to any node and pays the fee in KSA;
  path C carries it in a Bitcoin transaction of the account's own and pays only the Bitcoin fee.
- **Deployment.** A deployment stores the code and calls `init(init_args)` once, with the deployer as `caller()`.
  `init` can never be called again. The new contract's ID is `sha256(deployer ‖ nonce as u64 little-endian ‖
  sha256(code))`, which `sdk::contract_id` computes.
- **Calls.** Each call runs in a new instance: the method name and the argument bytes go in, `Ok(bytes)` or
  `Err(reason)` comes out. `caller()` is the signing account at the top level and the calling contract in a call from
  another contract.
- **All or nothing.** A revert, a trap or running out of gas, at any level of nested calls, fails the whole operation:
  every change and event is rolled back, and only the fee and the nonce stay spent. `try_call` is the only way to catch
  a callee's failure.
- **Reading.** A `view` runs on a node through the JSON-RPC method `ksana_call`: free, unsigned, and nothing it writes
  is kept.
- **KSA and assets.** KSA is the built-in asset whose ID is 32 zero bytes (`sdk::KSA_ASSET_ID`), with 8 decimals: 1 KSA
  is 100,000,000 base units. A contract can issue built-in assets of its own (Built-in assets, below). KS-20 tokens and
  KS-721 collections are contracts that keep their own ledgers (Token standards, below).
- **Networks.** Devnet has the chain ID 1001, the signet 1002 and mainnet 1000; `sdk::chain_id()` tells which.

## A complete contract

A poll, the contract of the docs' tutorial (https://ksana.network/docs/first-contract/), `contracts/examples/poll` in the
repository. The account that deploys it asks a question with 2 to 8 options; every account votes once while it is open,
and the creator closes it. Build it on the playground's Counter page.

```rust
//! Tutorial contract: a poll. The account that deploys it asks a question with 2 to 8 options; every account votes
//! once while the poll is open, and the creator closes it. It takes no funds.
//!
//! The contract of the docs' tutorial "Your first contract", which builds it in the playground in place of the
//! counter's source, so it needs nothing but the contract SDK. It keeps its state under a few storage keys and one key
//! per voter, refuses with a reason what it does not allow, knows its creator and its voters by `caller()`, and emits
//! an event for every change.
//!
//! Arguments and return values are Borsh-encoded; the options are numbered from 0 in the order `init` takes them.
//!
//! ```abi
//! init(question: String, options: Vec<String>)   // a question of 1 to 200 bytes; 2 to 8 options of 1 to 64 bytes
//! call vote(option: u32) -> u64                   // one vote per account while the poll is open; returns the option's votes
//! call close()                                    // the creator only: ends the voting for good
//! view poll() -> (String, Vec<String>, bool)      // the question, the options, whether voting is open
//! view results() -> Vec<u64>                      // the votes for each option, in order
//! view voted(account: Account) -> Option<u32>     // the option the account voted for; None before it votes
//! event voted(voter: Account, option: u32)
//! event closed(total: u64)
//! ```
#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::{self as sdk, storage, CallResult, Decode};

sdk::entry!(dispatch);

/// The longest question, in bytes.
pub const MAX_QUESTION: usize = 200;
/// The longest option, in bytes.
pub const MAX_OPTION: usize = 64;
/// The most options a poll has; it has at least 2.
pub const MAX_OPTIONS: usize = 8;

const KEY_QUESTION: &[u8] = b"question";
const KEY_OPTIONS: &[u8] = b"options";
const KEY_CREATOR: &[u8] = b"creator";
const KEY_OPEN: &[u8] = b"open";
const KEY_TALLY: &[u8] = b"tally";
/// Followed by a voter's account: the option it voted for.
const PREFIX_VOTE: &[u8] = b"vote:";

/// Dispatches the poll's methods; storage belongs to this contract instance.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    if !sdk::call_funds().is_empty() {
        return sdk::revert("unexpected attached funds");
    }
    match method {
        "init" => {
            let (question, options): (String, Vec<String>) = sdk::decode(args)?;
            init(question, options)
        }
        "vote" => vote(sdk::decode(args)?),
        "close" => {
            sdk::decode::<()>(args)?;
            close()
        }
        "poll" => {
            sdk::decode::<()>(args)?;
            let question: String = load(KEY_QUESTION)?;
            let options: Vec<String> = load(KEY_OPTIONS)?;
            let open: bool = load(KEY_OPEN)?;
            sdk::ok(&(question, options, open))
        }
        "results" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&load::<Vec<u64>>(KEY_TALLY)?)
        }
        "voted" => {
            let account: [u8; 32] = sdk::decode(args)?;
            sdk::ok(&storage::read::<u32>(&vote_key(&account))?)
        }
        _ => sdk::revert("unknown method"),
    }
}

/// Keeps the question and the options and opens the poll with no votes; the deploying account is its creator.
fn init(question: String, options: Vec<String>) -> CallResult {
    if question.is_empty() || question.len() > MAX_QUESTION {
        return sdk::revert("the question must be 1 to 200 bytes");
    }
    if options.len() < 2 || options.len() > MAX_OPTIONS {
        return sdk::revert("a poll has 2 to 8 options");
    }
    if options.iter().any(|o| o.is_empty() || o.len() > MAX_OPTION) {
        return sdk::revert("an option must be 1 to 64 bytes");
    }
    storage::write(KEY_TALLY, &alloc::vec![0u64; options.len()]);
    storage::write(KEY_QUESTION, &question);
    storage::write(KEY_OPTIONS, &options);
    storage::write(KEY_CREATOR, &sdk::caller());
    storage::write(KEY_OPEN, &true);
    sdk::ok(&())
}

/// Counts the caller's vote for `option`, once per account while the poll is open.
fn vote(option: u32) -> CallResult {
    if !load::<bool>(KEY_OPEN)? {
        return sdk::revert("voting is closed");
    }
    let voter = sdk::caller();
    let key = vote_key(&voter);
    if storage::get(&key).is_some() {
        return sdk::revert("already voted");
    }
    let mut tally: Vec<u64> = load(KEY_TALLY)?;
    let Some(count) = usize::try_from(option).ok().and_then(|i| tally.get_mut(i)) else {
        return sdk::revert("unknown option");
    };
    let Some(votes) = count.checked_add(1) else {
        return sdk::revert("too many votes");
    };
    *count = votes;
    storage::write(KEY_TALLY, &tally);
    storage::write(&key, &option);
    sdk::emit(&("voted", voter, option));
    sdk::ok(&votes)
}

/// Ends the voting; only the creator may, once.
fn close() -> CallResult {
    if load::<[u8; 32]>(KEY_CREATOR)? != sdk::caller() {
        return sdk::revert("only the creator");
    }
    if !load::<bool>(KEY_OPEN)? {
        return sdk::revert("voting is closed");
    }
    let tally: Vec<u64> = load(KEY_TALLY)?;
    let Some(total) = tally.iter().try_fold(0u64, |sum, n| sum.checked_add(*n)) else {
        return sdk::revert("too many votes");
    };
    storage::write(KEY_OPEN, &false);
    sdk::emit(&("closed", total));
    sdk::ok(&())
}

/// Reads a value `init` stored.
fn load<T: Decode>(key: &[u8]) -> Result<T, Vec<u8>> {
    storage::read(key)?.ok_or_else(|| b"not initialized".to_vec())
}

/// The storage key of `account`'s vote.
fn vote_key(account: &[u8; 32]) -> Vec<u8> {
    [PREFIX_VOTE, account.as_slice()].concat()
}
```

A native test of it, `tests/one_vote.rs`, runs on the SDK's mock host with `cargo test`:

```rust
//! A native test of the poll on the contract SDK's mock host.
#![cfg(not(target_arch = "wasm32"))]
#![allow(missing_docs, clippy::unwrap_used)]

use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode};

#[test]
fn every_account_votes_once() {
    mock::reset();
    let creator = [1; 32];
    let init = encode(&(
        "Which fruit?",
        vec!["apple".to_string(), "pear".to_string()],
    ));
    let contract = mock::deploy(creator, 0, b"poll", poll::dispatch, &init).unwrap();

    let votes: u64 =
        decode(&mock::call([2; 32], contract, "vote", &encode(&1u32)).unwrap()).unwrap();
    assert_eq!(votes, 1);
    assert_eq!(
        mock::call([2; 32], contract, "vote", &encode(&0u32)),
        Err(Failure::Revert {
            contract,
            reason: b"already voted".to_vec()
        })
    );
    let results: Vec<u64> =
        decode(&mock::call([9; 32], contract, "results", &[]).unwrap()).unwrap();
    assert_eq!(results, vec![0, 1]);
    assert_eq!(mock::events().len(), 1);
}
```

## Set up, test and build locally

You need Rust 1.98.1 with the `wasm32-unknown-unknown` target, and a clone of the repository: the SDK is not on
crates.io. Put the project next to the clone, not inside it, so that Cargo does not take it for a member of the
repository's workspace (`work/Ksana/` and `work/poll/`):

```toml
# Cargo.toml
[package]
name = "poll"
version = "0.1.0"
edition = "2021"
rust-version = "1.98.1"
publish = false

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
ksana-contract-sdk = { path = "../Ksana/crates/ksana-contract-sdk" }
# a contract built on the token standards (ks20, ks721, standard) adds: features = ["standards"]

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true

# the lints the repository's contracts use: run cargo clippy -- -D warnings, natively and with --target wasm32-unknown-unknown
[lints.rust]
unsafe_code = "deny"
missing_docs = "warn"

[lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
unreachable = "deny"
todo = "deny"
unimplemented = "deny"
indexing_slicing = "deny"
arithmetic_side_effects = "deny"
float_arithmetic = "deny"
cast_possible_truncation = "deny"
cast_possible_wrap = "deny"
cast_sign_loss = "deny"
```

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.98.1"
targets = ["wasm32-unknown-unknown"]
```

```sh
cargo test                                                                  # native tests on the mock host
cargo rustc --release --target wasm32-unknown-unknown --crate-type cdylib   # target/wasm32-unknown-unknown/release/poll.wasm
```

Build the module as a `cdylib` alone: `cargo build` also builds the `rlib`, and the module comes out larger. Test files
allow `unwrap_used` and `panic` again at their top. The mock host follows the VM's rules and limits but meters no gas;
KSA exists in it only after `mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32])` and `mock::mint`, and the `code`
given to `mock::deploy` is a label, so contract IDs in tests differ from those on chain. The module's bytes depend on
the path of the SDK checkout, so compute a contract's ID from the very file you deploy.

## The playground

The playground (https://playground.ksana.network/) compiles and deploys contracts on the signet from the browser, with
nothing to install:

1. Connect a wallet (Unisat, OneKey or Xverse) with a Taproot account on Bitcoin signet. The faucet sends 100 test KSA
   a day to each account.
2. Open an example's **Code** tab, replace the source with yours and press **Compile**. Errors and warnings come back
   on their lines and in a list.
3. Press **Deploy this build**. The dialog asks for `init`'s arguments in a form built from your ABI block.
4. The **Methods** tab runs each `view` read-only and sends each `call` as an operation you sign in your wallet, with
   forms, results and events decoded by the ABI block.

The compile service builds your source as the `src/lib.rs` of one example crate, with that crate's dependencies and no
others:

| Page | Crate | SDK features |
| --- | --- | --- |
| Counter, `#/counter/code` | `playground-counter` | none |
| Asset, `#/token/code` | `mintable-token` | none |
| KS-20 token, `#/ks20/code` | `token` | `standards` |
| NFT, `#/nft/code` | `nft` | `standards` |
| AMM, `#/amm/code` | `amm` | `standards` |
| Lending, `#/lending/code` | `lending` | none |
| Multisig, `#/multisig/code` | `multisig` | none |
| Vesting, `#/vesting/code` | `vesting` | none |

A contract that uses `ks20`, `ks721` or `standard` compiles on the KS-20 token, NFT or AMM page, any other on the Counter
page. A source is at most 128 KiB, a build stops after 120 seconds, and one address gets 6 builds a minute. The module
must fit in one operation: 65,536 bytes with the rest of it.

## The ABI block

A contract describes its interface to the playground in a fenced block marked `abi` inside the `//!` documentation at
the top of `src/lib.rs`, one declaration per line, each with an optional `//` description after it:

```text
//! ```abi
//! struct Funds { asset: AssetId, amount: u128 }
//! enum Action { Transfer(AssetId, Account, u128), SetThreshold(u8) }
//! init(name: String, decimals: u8)                   // issues the supply to the deployer
//! payable call deposit(min_shares: u128) -> u128
//! view balance_of(account: Account) -> u128
//! event transfer(from: Account, to: Account, amount: u128)
//! ```
```

- `init(…)` gives the deployment's arguments and returns nothing. `call` declares a method that may change state, sent
  as a signed operation; `view` one that only reads, run with `ksana_call`, which must return a value. `payable` before
  `init` or `call` marks a method that takes attached funds (never a `view`), and `-> T` the return type (none for
  `()`). Every parameter has a name.
- `struct Name { field: Type, … }` and `enum Name { Variant, Variant(Type, …), … }` declare the types the methods use,
  each on one line, in Borsh's layout: a struct's fields in order; an enum's `u8` tag in the order of its variants,
  then the variant's fields. A variant has no named fields. `event name(fields)` declares an event emitted as the Borsh
  tuple `("name", fields…)`.
- Types: `u8`, `u16`, `u32`, `u64`, `u128`, `i8`, `i16`, `i32`, `i64`, `i128`, `bool`, `String`, `Bytes` (a
  `Vec<u8>`), `[u8; N]`, the 32-byte `Account`, `ContractId`, `AssetId`, `Hash` and `TokenId` (each a `[u8; 32]` in
  Rust), `Option<T>`, `Vec<T>`, tuples such as `(String, u64)`, `()`, and the structs and enums the block declares.
  An `Option` directly inside an `Option` is refused: its `Some(None)` would read as `None`.

## Arguments as bytes

An operation carries a method's arguments as bytes, and `ksana-cli` and `ksana_call` take them in hex. They are a Borsh
tuple: the encodings of the parameters back to back. One parameter is its own encoding, none is empty bytes, and a
return value follows the same rules.

| Type | Borsh bytes |
| --- | --- |
| `u8` … `u128`, `i8` … `i128` | little-endian, 1 to 16 bytes: `vote(1)` is `01000000` |
| `bool` | `00` or `01` |
| `String` | u32 length, little-endian, then the UTF-8: `"pear"` is `0400000070656172` |
| `Vec<T>`, `Bytes` | u32 count, little-endian, then each element |
| `[u8; N]`, `Account`, `ContractId`, `AssetId`, `Hash`, `TokenId` | the N bytes, with no length |
| `Option<T>` | `00`, or `01` then `T` |
| tuple, struct | the fields in order |
| enum | the variant's index as one byte, then its fields |

The poll's `init("Which fruit?", ["apple", "pear"])` is
`0c00000057686963682066727569743f02000000050000006170706c650400000070656172`.

## Deploy and call from the command line

`ksana-cli` is built from the repository (`cargo build --release --locked -p ksana-cli`). It takes arguments as the hex
of their Borsh bytes, amounts in base units, and `--fee-token`, the path B fee, in KSA base units (`ksana-cli
estimate-fee` suggests one).

```sh
export KSANA_RPC=https://signet.ksana.network/rpc
ksana-cli --chain signet key alice.key    # makes the key file once; prints the account and its tb1p… address
ksana-cli deploy --key alice.key --code poll.wasm \
  --init-args 0c00000057686963682066727569743f02000000050000006170706c650400000070656172 \
  --fee-token 1 --wait > deploy.json      # the operation's account, nonce and status
POLL=$(python3 -c 'import hashlib, json, sys
d = json.load(open("deploy.json")); code = hashlib.sha256(open("poll.wasm", "rb").read()).digest()
print(hashlib.sha256(bytes.fromhex(d["account"]) + int(d["nonce"]).to_bytes(8, "little") + code).hexdigest())')
ksana-cli call --key alice.key --contract $POLL --method vote --args 01000000 --fee-token 1 --wait
ksana-cli query --contract $POLL --method results     # read-only, free
```

`deploy` prints the operation's `account` and `nonce`, not the contract ID, which is `sha256(account ‖ nonce as u64
little-endian ‖ sha256(module))`, as above. `--fund <amount>` attaches KSA and `--fund <asset ID>:<amount>` another
asset, to `deploy`, `call` and `query`. `--max-gas` is 1,000,000 by default: a deployment costs about 100,000 plus 21
per byte of the module plus its `init`, so raise it for a module over about 40,000 bytes.

From an app, the JSON-RPC method `ksana_call` runs a method read-only. Its one parameter is
`{ "contract", "method", "args", "funds"?, "caller"?, "max_gas"? }`: hex for the contract and the arguments (`""` for
none), `[{ "asset", "amount" }]` for funds, amounts and `max_gas` as decimal strings; `caller` is 32 zero bytes unless
given. It answers a receipt: `{ "op_id", "success", "error", "gas_used", "output", "events": [{ "contract", "data" }],
"asset_changes" }`, with `output` and each event's `data` in hex. The TypeScript SDK `@ksana/sdk` (`sdk/ts`) signs and
sends operations and makes these calls (https://ksana.network/docs/sdk/).

## When an operation fails

The receipt's `error` says why, and a failed operation's `gas_used` is its whole `max_gas`:

- `revert: <reason>`: the contract, or a contract it called, returned `Err(reason)`.
- `out of gas`: it needed more than `max_gas`.
- `trap: <what happened>`: a panic, an out-of-bounds access, a division by zero, or a host function asked to go past a
  limit.
- `insufficient balance` and other bare messages: a host function refused, such as an `asset_transfer` the contract's
  balance cannot cover.
- `rejected: <reason>`: a built-in asset's rules refused a movement, such as `rejected: asset is paused`; a token
  contract's own rules use the same prefix inside a revert.
- `deployment rejected: <reason>`: the module failed the checks at deployment (floating point, a feature outside
  WebAssembly 1.0 and its allowed extensions, an import that is not a host function).

Run the same call with `ksana-cli query` or `ksana_call` first: it shows the error for nothing.

## Gas

Every WebAssembly instruction costs about 1 gas, and each SDK call that reaches the host adds:

| SDK call | Gas |
| --- | --- |
| `storage::get`, `storage::read` | 2,000 + 1 per byte of key and value |
| `storage::set`, `storage::write` | 10,000 + 10 per byte of key and value |
| `storage::remove` | 5,000 + 1 per byte of key |
| `caller`, `self_id`, `block_height`, `block_timestamp`, `chain_id`, `gas_left`, `btc_anchor_height` | 100 each |
| `btc_header` | 2,000 |
| `native_tx` | 2,000 + 1 per byte returned |
| `asset_balance`, `asset_policy`, `asset_status` | 2,000 |
| `asset_transfer`, `asset_burn` | 10,000 |
| `asset_issue`, `asset_create` | 20,000 + 1 per byte of the Borsh `AssetMetaInput` |
| `asset_freeze`, `asset_allow`, `asset_pause`, `asset_renounce` | 10,000 |
| `call_funds`, `received` | 100 + 1 per byte of the Borsh `Vec<Funds>` |
| `call`, `call_with_funds`, `try_call`, `try_call_with`, `delegate_call` | 20,000 + 1 per byte of input + 1 per byte of the callee's code + 10,000 per attached asset, and what the callee uses |
| `create_contract` | 100,000 + 1 per byte of `init_args` + 1 per byte of code + 10,000 per attached asset, and what `init` uses |
| `emit`, `emit_event` | 1,000 + 5 per byte |
| `sha256` | 500 + 1 per byte |
| `sha256d` | 1,032 + 1 per byte (two `sha256`) |
| `asset_id`, `contract_id`, `created_contract_id` | 564, 572 and 596 (one `sha256` each) |
| `verify_schnorr` | 50,000 |

An operation adds its own cost to what its contract code uses:

| Operation | Gas |
| --- | --- |
| Deploy | 100,000 + 20 per byte of code + 1 per byte of code to load it + 10,000 per attached asset, and what `init` uses |
| Call | 20,000 + 1 per byte of the contract's code + 10,000 per attached asset, and what the method uses |
| Asset transfer | 10,000 |

A path B fee is ranked by fee per KiB of the operation's cost, the larger of its size and its `max_gas` in bytes, so
set `max_gas` near what the operation needs; the playground does so by running each call read-only first.

## Limits

| Limit | Value |
| --- | --- |
| Storage key | 256 bytes |
| Storage value | 65,536 bytes |
| Event | 16,384 bytes |
| Call input (method name and arguments) | 65,536 bytes |
| Return value | 65,536 bytes |
| Assets attached to one call | 16 |
| Nested calls, the operation's own call the first | 8 levels |
| Memory of one instance | 16 MiB |
| Stack of one instance, by the VM's estimate | 262,144 bytes |
| Instances in one operation | 64 |
| Memory of all instances in one operation | 256 MiB |
| Table entries of a module | 4,096 |
| Operation, code included | 65,536 bytes |
| Gas of one operation | 100,000,000; 1,000,000 on path C |
| Bitcoin headers a contract reads | up to `btc_anchor_height() - 6` |
| Asset name, symbol, decimals | 1 to 64 bytes, 1 to 16 bytes, at most 18 |
| Asset metadata, the Borsh `AssetMetaInput` | 1,024 bytes |

These limits are the same on devnet, the signet and mainnet. A host function asked to go past one traps.

## The contract SDK

`ksana_contract_sdk`, imported as `use ksana_contract_sdk::{self as sdk, storage, CallResult};`. On
`wasm32-unknown-unknown` it is `no_std` and calls the host; on other targets the same functions run on the in-memory
mock host. Host functions trap on misuse (a key too long, a balance too small, a missing asset, an asset's rules),
which the contract cannot catch, so check first and revert with a reason. `sdk::entry!(dispatch)` exports the module's entry points;
`ksana_contract_sdk::borsh` re-exports Borsh for `#[derive(BorshSerialize)]` with
`#[borsh(crate = "ksana_contract_sdk::borsh")]`; decoding uses this crate's `Decode`, which a struct implements by
hand, field by field (`Some(Self { a: u64::decode(input)?, b: <[u8; 32]>::decode(input)? })`).

### Call context and encoding

```rust
// ksana_contract_sdk
/// What a method returns: `Ok(data)`, or `Err(reason)` to revert.
pub type CallResult = Result<Vec<u8>, Vec<u8>>;
/// Decodes all of `bytes` as `T`; `Err(ERR_MALFORMED_ARGS)` when they do not fit, for `?`.
pub fn decode<T: Decode>(bytes: &[u8]) -> Result<T, Vec<u8>>;
/// Borsh-encodes `value`.
pub fn encode<T: BorshSerialize + ?Sized>(value: &T) -> Vec<u8>;
/// `Ok(encode(value))`.
pub fn ok<T: BorshSerialize + ?Sized>(value: &T) -> CallResult;
/// `Err(reason)`: reverts the whole operation with `reason`.
pub fn revert<T>(reason: &str) -> Result<T, Vec<u8>>;
pub const ERR_MALFORMED_ARGS: &[u8] = b"malformed arguments";
/// Borsh decoding without `core::fmt`; implemented for the integers, `bool` (0 or 1), `[u8; N]`, `Vec<T>`, `String`,
/// `Option<T>`, `Result<T, E>`, `()`, tuples of up to 8 elements, `Funds`, `AssetMetaInput`, `NativeTx`,
/// `NativeOutput` and `standard::AssetRef`.
pub trait Decode: Sized {
    fn decode(input: &mut &[u8]) -> Option<Self>;
}
/// The caller: the signing account at the top level, the calling contract in a call from a contract.
pub fn caller() -> [u8; 32];
/// This contract's ID.
pub fn self_id() -> [u8; 32];
/// The height of the block running the operation.
pub fn block_height() -> u64;
/// The block's timestamp in Unix seconds, set by its producer within the consensus rules: not a precise time.
pub fn block_timestamp() -> u64;
/// 1001 devnet, 1002 signet, 1000 mainnet.
pub fn chain_id() -> u32;
/// The gas left to this call.
pub fn gas_left() -> u64;
/// Aborts the operation (a trap, without a reason); revert instead whenever there is one.
pub fn trap() -> !;
/// Records an event (raw bytes); events are rolled back with a failed operation.
pub fn emit_event(data: &[u8]);
/// Records the Borsh encoding of `event`, by convention a tuple whose first element is the event's name.
pub fn emit<T: BorshSerialize + ?Sized>(event: &T);
pub fn sha256(data: &[u8]) -> [u8; 32];
/// `sha256(sha256(data))`, as Bitcoin hashes.
pub fn sha256d(data: &[u8]) -> [u8; 32];
/// BIP340: a 32-byte x-only key, a 32-byte message digest, a 64-byte signature.
pub fn verify_schnorr(pubkey: &[u8; 32], msg: &[u8; 32], sig: &[u8; 64]) -> bool;
/// `sha256(deployer ‖ nonce as u64 little-endian ‖ code_hash)`: the ID of a contract an account deployed.
pub fn contract_id(deployer: &[u8; 32], nonce: u64, code_hash: &[u8; 32]) -> [u8; 32];
```

### Built-in assets and attached funds

```rust
// ksana_contract_sdk
/// KSA's asset ID: 32 zero bytes.
pub const KSA_ASSET_ID: [u8; 32] = [0u8; 32];
/// An asset and an amount attached to a call; Borsh-encodable, `Decode`.
pub struct Funds { pub asset: [u8; 32], pub amount: u128 }
/// The most assets one call attaches.
pub const MAX_CALL_FUNDS: usize = 16;
/// The funds attached to this call, already in this contract's balance, in ascending asset ID order.
pub fn call_funds() -> Vec<Funds>;
/// The amount of `asset` attached to this call; 0 when none.
pub fn received(asset: &[u8; 32]) -> u128;
pub fn asset_balance(asset: &[u8; 32], account: &[u8; 32]) -> u128;
/// Pays `amount` of `asset` from this contract's balance to `to`; traps when the balance is short.
pub fn asset_transfer(asset: &[u8; 32], to: &[u8; 32], amount: u128);
/// Burns `amount` of this contract's own balance of `asset`, KSA included.
pub fn asset_burn(asset: &[u8; 32], amount: u128);
/// A new asset's name (1 to 64 bytes), symbol (1 to 16 bytes) and decimals (at most 18); Borsh-encodable, `Decode`.
pub struct AssetMetaInput { pub name: String, pub symbol: String, pub decimals: u8 }
/// Issues `amount` of this contract's asset `sub_id` to `to` and returns its ID. The first issue creates the asset with
/// `CAP_MINT` alone and records `meta`; every later one passes the same `meta` and needs `CAP_MINT` still.
pub fn asset_issue(sub_id: &[u8; 32], meta: &AssetMetaInput, to: &[u8; 32], amount: u128) -> [u8; 32];
/// Creates this contract's asset `sub_id` with the issuer capabilities `capabilities` (`CAP_*` bits) and issues
/// `amount` of it (0 for none) to `to`; traps when the asset exists. Without `CAP_MINT` the supply stays `amount` for
/// good; with `CAP_ALLOWLIST`, `to` must be this contract until it allows an account.
pub fn asset_create(sub_id: &[u8; 32], meta: &AssetMetaInput, capabilities: u8, to: &[u8; 32], amount: u128) -> [u8; 32];
/// `sha256(issuer ‖ sub_id)`: the ID of the asset a contract issues.
pub fn asset_id(issuer: &[u8; 32], sub_id: &[u8; 32]) -> [u8; 32];
/// Issuer capabilities, declared by `asset_create` and only ever given up.
pub const CAP_MINT: u8 = 1;
pub const CAP_FREEZE: u8 = 2;
pub const CAP_ALLOWLIST: u8 = 4;
pub const CAP_PAUSE: u8 = 8;
/// Freezes or unfreezes `account` for this contract's asset, never this contract itself (freezing needs `CAP_FREEZE`):
/// a frozen account can neither send, receive nor burn it.
pub fn asset_freeze(sub_id: &[u8; 32], account: &[u8; 32], frozen: bool);
/// Puts `account` on or off the allowlist of this contract's asset (putting it on needs `CAP_ALLOWLIST`).
pub fn asset_allow(sub_id: &[u8; 32], account: &[u8; 32], allowed: bool);
/// Pauses or resumes this contract's asset (pausing needs `CAP_PAUSE`).
pub fn asset_pause(sub_id: &[u8; 32], paused: bool);
/// Gives up capabilities of this contract's asset for good.
pub fn asset_renounce(sub_id: &[u8; 32], capabilities: u8);
/// The capabilities an asset's issuer still holds and whether it is paused.
pub struct AssetPolicy { pub capabilities: u8, pub paused: bool }
/// `None` when the asset does not exist; KSA has no capabilities.
pub fn asset_policy(asset: &[u8; 32]) -> Option<AssetPolicy>;
/// An account's `STATUS_*` flags for an asset.
pub fn asset_status(asset: &[u8; 32], account: &[u8; 32]) -> u8;
pub const STATUS_FROZEN: u8 = 1;
pub const STATUS_ALLOWED: u8 = 2;
```

### Calling and creating contracts

```rust
// ksana_contract_sdk
/// Calls `method` of `contract` and returns the bytes in its `Ok`; any failure of the callee fails the operation.
/// The method name `init` is refused.
pub fn call(contract: &[u8; 32], method: &str, args: &[u8]) -> Vec<u8>;
/// As `call`, attaching `funds` from this contract's balance (strictly ascending asset IDs, amounts above 0).
pub fn call_with_funds(contract: &[u8; 32], method: &str, args: &[u8], funds: &[Funds]) -> Vec<u8>;
/// How a caught call failed: the callee's revert reason, or any other failure.
pub enum CallError { Revert(Vec<u8>), Failed }
/// As `call`, but a failure undoes only the callee's level and comes back as `Err`. The callee gets at most 63/64 of
/// the gas left, and a failure other than a revert uses up what it got.
pub fn try_call(contract: &[u8; 32], method: &str, args: &[u8]) -> Result<Vec<u8>, CallError>;
/// `try_call` with attached funds and a gas limit for the callee (0: no limit beyond 63/64).
pub fn try_call_with(contract: &[u8; 32], method: &str, args: &[u8], funds: &[Funds], gas: u64) -> Result<Vec<u8>, CallError>;
/// Runs `contract`'s code in this contract's context (its storage, balances, caller and funds): trusted code only.
pub fn delegate_call(contract: &[u8; 32], method: &str, args: &[u8]) -> Vec<u8>;
/// Creates a contract from code already on chain (`code_hash = sha256(code)`, stored by an earlier deployment) and runs
/// its `init(init_args)` with this contract as the caller, `funds` moved to it first; returns
/// `created_contract_id(self_id(), salt, code_hash)`. Fails the operation when the code is unknown, the ID is taken or
/// `init` fails.
pub fn create_contract(code_hash: &[u8; 32], salt: &[u8; 32], init_args: &[u8], funds: &[Funds]) -> [u8; 32];
/// `sha256(creator ‖ salt ‖ code_hash)`.
pub fn created_contract_id(creator: &[u8; 32], salt: &[u8; 32], code_hash: &[u8; 32]) -> [u8; 32];
```

### Bitcoin

```rust
// ksana_contract_sdk
/// The Bitcoin height the current block anchors to.
pub fn btc_anchor_height() -> u32;
/// The 80-byte Bitcoin header at `height`, which must be at most `btc_anchor_height() - 6`; traps otherwise.
pub fn btc_header(height: u32) -> [u8; 80];
/// An output of the Bitcoin transaction carrying a path C operation.
pub struct NativeOutput { pub value: u64, pub script: Vec<u8> }
/// That transaction: its ID (internal byte order) and every output.
pub struct NativeTx { pub txid: [u8; 32], pub outputs: Vec<NativeOutput> }
/// The transaction carrying this operation when it came by path C; `None` for path B and read-only calls. Every level
/// of one operation reads the same transaction, so record what it paid for to credit it once.
pub fn native_tx() -> Option<NativeTx>;
```

```rust
// ksana_contract_sdk::btc
pub const HEADER_LEN: usize = 80;
pub const MAX_MONEY: u64 = 21_000_000 * 100_000_000;
pub const MAX_COMPACT_SIZE: u64 = 0x0200_0000;
pub const MAX_MERKLE_DEPTH: usize = 32;
/// A parsed block header; hashes in Bitcoin's internal byte order.
pub struct BlockHeader { pub version: i32, pub prev_blockhash: [u8; 32], pub merkle_root: [u8; 32], pub time: u32, pub bits: u32, pub nonce: u32 }
impl BlockHeader {
    pub fn parse(bytes: &[u8]) -> Option<Self>;
    pub fn to_bytes(&self) -> [u8; HEADER_LEN];
    pub fn block_hash(&self) -> [u8; 32];
}
/// An output of a parsed transaction.
pub struct TxOut<'a> { pub value: u64, pub script_pubkey: &'a [u8] }
/// `sha256d` of a transaction serialized without witness data.
pub fn txid(raw_tx: &[u8]) -> [u8; 32];
/// The outputs of a transaction serialized without witness data; `None` when it is malformed.
pub fn parse_outputs(raw_tx: &[u8]) -> Option<Vec<TxOut<'_>>>;
pub fn merkle_root_from_branch(leaf: &[u8; 32], index: u32, branch: &[[u8; 32]]) -> Option<[u8; 32]>;
pub fn verify_merkle_branch(leaf: &[u8; 32], index: u32, branch: &[[u8; 32]], root: &[u8; 32]) -> bool;
pub fn is_coinbase(raw_tx: &[u8]) -> bool;
/// Proves `raw_tx` is transaction `index` of the block `header` and returns its ID; the same block's coinbase and its
/// branch pin the tree's depth. Guard replays by the returned ID, never by `index`.
pub fn verify_tx_inclusion(header: &BlockHeader, raw_tx: &[u8], index: u32, branch: &[[u8; 32]], coinbase_tx: &[u8], coinbase_branch: &[[u8; 32]]) -> Option<[u8; 32]>;
```

### Storage

```rust
// ksana_contract_sdk::storage
/// The raw value of `key`, `None` when absent.
pub fn get(key: &[u8]) -> Option<Vec<u8>>;
pub fn set(key: &[u8], value: &[u8]);
pub fn remove(key: &[u8]);
/// Reads and Borsh-decodes `key`: `Ok(None)` when absent, `Err(ERR_MALFORMED_STORAGE)` when it does not decode.
pub fn read<T: Decode>(key: &[u8]) -> Result<Option<T>, Vec<u8>>;
/// Borsh-encodes `value` under `key`.
pub fn write<T: BorshSerialize + ?Sized>(key: &[u8], value: &T);
pub const ERR_MALFORMED_STORAGE: &[u8] = b"malformed storage value";
```

### Native tests: the mock host

`mock` exists on non-wasm32 targets only. Its state is per thread; call `mock::reset()` at the start of each test.

```rust
// ksana_contract_sdk::mock
/// Clears everything.
pub fn reset();
/// Deploys: the ID from `deployer`, `nonce` and `sha256(code)` (`code` is a label, not the module), then `init` as
/// `deployer`; a failed `init` deploys nothing.
pub fn deploy(deployer: [u8; 32], nonce: u64, code: &[u8], dispatch: impl Fn(&str, &[u8]) -> CallResult + 'static, init_args: &[u8]) -> Result<[u8; 32], Failure>;
pub fn deploy_with_funds(deployer: [u8; 32], nonce: u64, code: &[u8], dispatch: impl Fn(&str, &[u8]) -> CallResult + 'static, init_args: &[u8], funds: &[Funds]) -> Result<[u8; 32], Failure>;
/// An operation of `caller` calling `contract`; a failure at any level rolls everything back.
pub fn call(caller: [u8; 32], contract: [u8; 32], method: &str, args: &[u8]) -> Result<Vec<u8>, Failure>;
pub fn call_with_funds(caller: [u8; 32], contract: [u8; 32], method: &str, args: &[u8], funds: &[Funds]) -> Result<Vec<u8>, Failure>;
/// Why an operation failed.
pub enum Failure { Revert { contract: [u8; 32], reason: Vec<u8> }, Trap(String) }
/// A dispatch function for `contract` without `init`, such as a stub of another contract.
pub fn register(contract: [u8; 32], dispatch: impl Fn(&str, &[u8]) -> CallResult + 'static);
/// Code that `create_contract` can create from; returns its hash.
pub fn register_code(code: &[u8], dispatch: impl Fn(&str, &[u8]) -> CallResult + 'static) -> [u8; 32];
pub type Dispatch = Rc<dyn Fn(&str, &[u8]) -> CallResult>;
/// Runs `f` as `caller` calling `self_id`, without rollback, to test a contract's internals.
pub fn with_context<R>(caller: [u8; 32], self_id: [u8; 32], f: impl FnOnce() -> R) -> R;
pub fn contract_exists(contract: &[u8; 32]) -> bool;
pub fn storage_value(contract: &[u8; 32], key: &[u8]) -> Option<Vec<u8>>;
/// An event a successful operation emitted.
pub struct Event { pub contract: [u8; 32], pub data: Vec<u8> }
pub fn events() -> Vec<Event>;
pub fn clear_events();
/// Registers an asset with no supply (an issuer of 32 zero bytes, as KSA's, gets no capabilities, any other `CAP_MINT`).
pub fn register_asset(asset: [u8; 32], name: &str, decimals: u8, issuer: [u8; 32]);
/// Gives `account` `amount` of a registered asset.
pub fn mint(asset: [u8; 32], account: [u8; 32], amount: u128) -> Result<(), Failure>;
pub fn balance(asset: &[u8; 32], account: &[u8; 32]) -> u128;
pub struct AssetInfo { pub name: String, pub symbol: String, pub decimals: u8, pub total_supply: u128, pub issuer: [u8; 32], pub capabilities: u8, pub paused: bool }
pub fn asset_info(asset: &[u8; 32]) -> Option<AssetInfo>;
pub fn set_asset_policy(asset: [u8; 32], capabilities: u8, paused: bool);
pub fn set_account_status(asset: [u8; 32], account: [u8; 32], status: u8);
pub fn account_status(asset: &[u8; 32], account: &[u8; 32]) -> u8;
pub fn set_block_height(height: u64);
pub fn set_block_timestamp(timestamp: u64);
pub fn set_chain_id(chain_id: u32);
pub fn set_gas_left(gas: u64);
pub fn set_btc_anchor_height(height: u32);
pub fn set_btc_header(height: u32, header: [u8; 80]);
pub fn set_btc_query_depth(depth: u32);
pub fn set_max_call_depth(depth: usize);
/// `Some` makes the following operations path C operations carried by `tx`.
pub fn set_native_tx(tx: Option<NativeTx>);
pub const DEFAULT_CHAIN_ID: u32 = 1001;
pub const DEFAULT_GAS_LEFT: u64 = 1_000_000_000;
pub const DEFAULT_BTC_QUERY_DEPTH: u32 = 6;
pub const DEFAULT_MAX_CALL_DEPTH: usize = 8;
pub const MAX_STORAGE_KEY_BYTES: usize = 256;
pub const MAX_STORAGE_VALUE_BYTES: usize = 65_536;
pub const MAX_EVENT_BYTES: usize = 16_384;
pub const MAX_OUTPUT_BYTES: usize = 65_536;
pub const MAX_OP_BYTES: usize = 65_536;
pub const MAX_ASSET_META_BYTES: usize = 1_024;
pub const MAX_ASSET_NAME_BYTES: usize = 64;
pub const MAX_ASSET_SYMBOL_BYTES: usize = 16;
pub const MAX_ASSET_DECIMALS: u8 = 18;
```

## Built-in assets

A contract issues built-in assets as the VM keeps them: in the same balance table as KSA, moved by an asset transfer
operation or attached to calls, and listed by every node and wallet without an index. Its asset `sub_id` has the ID
`asset_id(self_id(), sub_id)`, and only it manages the asset. `asset_issue` creates the asset on first use with the
capability `mint` alone and issues more later; `asset_create` declares its issuer capabilities once (`CAP_MINT`,
`CAP_FREEZE`, `CAP_ALLOWLIST`, `CAP_PAUSE`), and `asset_renounce` can only give them up. Nothing ever moves a holder's
balance but the holder: there is no clawback. A movement the asset's rules refuse fails the operation with a reason
that starts with `rejected: `, such as `rejected: asset is paused`; the issuing contract itself is exempt from them.

Pieces of a contract that creates an asset in `init`, mints more and freezes accounts (its own methods check who may):

```rust
use ksana_contract_sdk::{self as sdk, AssetMetaInput, CAP_FREEZE, CAP_MINT};

/// The sub-ID of this contract's asset; the asset's ID is `sdk::asset_id(&sdk::self_id(), &POINTS)`.
const POINTS: [u8; 32] = [0; 32];

/// The asset's metadata, which every later issue passes again.
fn points() -> AssetMetaInput {
    AssetMetaInput {
        name: "Points".into(),
        symbol: "PTS".into(),
        decimals: 2,
    }
}

/// In `init`: creates the asset, able to mint more and to freeze accounts, with `supply` for the deployer.
pub fn create_points(supply: u128) -> [u8; 32] {
    sdk::asset_create(
        &POINTS,
        &points(),
        CAP_MINT | CAP_FREEZE,
        &sdk::caller(),
        supply,
    )
}

/// Issues `amount` more to `to`; the method that calls it checks who may.
pub fn mint_points(to: &[u8; 32], amount: u128) -> [u8; 32] {
    sdk::asset_issue(&POINTS, &points(), to, amount)
}

/// Freezes `account`, or lifts its freeze: a frozen account can neither send, receive nor burn the points.
pub fn freeze_points(account: &[u8; 32], frozen: bool) {
    sdk::asset_freeze(&POINTS, account, frozen);
}
```

## Token standards: KS-20 and KS-721

A KS-20 token or a KS-721 collection is a contract that keeps its own ledger, with fixed methods, events and revert
reasons, so that wallets, markets and pools handle any of them. The SDK implements both behind the feature `standards`;
in the playground, build on the KS-20 token, NFT or AMM page. Every one declares itself in `init`: the event `standards`
first, then `metadata`, and the view `standards()` returns the same list (`"KS-20"` or `"KS-721"` first, then the
extensions). Every method but `transfer_and_call` (and a receiver's `on_*_received`) refuses attached funds, and a
refusal by a token's own rules starts with `rejected: `.

```abi
// KS-20: amounts are u128 base units
view standards() -> Vec<String>                                         // ["KS-20", extensions...]
view metadata() -> (String, String, u8)                                 // (name, symbol, decimals)
view capabilities() -> Vec<String>                                      // among mint, freeze, allowlist, pause, clawback, fee, upgrade
view total_supply() -> u128
view balance_of(account: Account) -> u128
view allowance(owner: Account, spender: Account) -> u128                // what spender may take now: 0 once expired
view allowance_info(owner: Account, spender: Account) -> (u128, Option<u64>)   // (amount, expires_at) as set
call transfer(to: Account, amount: u128)
call transfer_from(from: Account, to: Account, amount: u128)            // with the allowance from gave the caller
call approve(spender: Account, amount: u128, expires_at: Option<u64>)   // sets, not adds; 0 revokes; u128::MAX never goes down
payable call transfer_and_call(to: ContractId, amount: u128, data: Bytes) -> Bytes   // returns the recipient's answer
event standards(ids: Vec<String>)
event metadata(name: String, symbol: String, decimals: u8)
event transfer(from: Account, to: Account, amount: u128)               // a mint is from the zero account, a burn to it
event approve(owner: Account, spender: Account, amount: u128, expires_at: Option<u64>)
// KS-20-burnable
call burn(amount: u128)
call burn_from(from: Account, amount: u128)
// KS-20-mintable
view minter() -> Option<Account>                                        // None once minting ended for good
view cap() -> Option<u128>                                              // None for no cap
call mint(to: Account, amount: u128)                                    // the minter only, up to the cap
call set_minter(minter: Option<Account>)                                // the minter only; None ends minting
event minter(previous: Option<Account>, minter: Option<Account>)
// a contract that takes tokens in one step
payable call on_token_received(from: Account, amount: u128, data: Bytes) -> Bytes
```

```abi
// KS-721: a token ID is 32 bytes; serial number n is ks721::id(n), n big-endian in the last 16 bytes
view standards() -> Vec<String>                                         // ["KS-721", extensions...]
view metadata() -> (String, String)                                     // (name, symbol)
view capabilities() -> Vec<String>
view owner_of(token_id: TokenId) -> Option<Account>                    // None for a token that does not exist
view balance_of(owner: Account) -> u64
view token_uri(token_id: TokenId) -> Option<String>
view approved(token_id: TokenId) -> Option<Account>
view is_operator(owner: Account, operator: Account) -> bool             // false once expired
view operator_info(owner: Account, operator: Account) -> (bool, Option<u64>)
call transfer(to: Account, token_id: TokenId)                          // the owner only
call transfer_from(from: Account, to: Account, token_id: TokenId)      // the owner, the approved account or an operator
call approve(spender: Option<Account>, token_id: TokenId)              // the owner or an operator; None clears
call set_operator(operator: Account, approved: bool, expires_at: Option<u64>)
payable call transfer_and_call(to: ContractId, token_id: TokenId, data: Bytes) -> Bytes   // the owner only
event standards(ids: Vec<String>)
event metadata(name: String, symbol: String)
event transfer(from: Account, to: Account, token_id: TokenId)
event approve(owner: Account, spender: Option<Account>, token_id: TokenId)
event operator(owner: Account, operator: Account, approved: bool, expires_at: Option<u64>)
event uri(token_id: Option<TokenId>)
// KS-721-burnable
call burn(token_id: TokenId)                                            // the owner, the approved account or an operator
// KS-721-enumerable: pages of at most 100
view total_supply() -> u64
view tokens(start: u64, limit: u32) -> Vec<TokenId>
view tokens_of(owner: Account, start: u64, limit: u32) -> Vec<TokenId>
// minting and init are the collection's own methods; a contract that takes tokens in one step
payable call on_nft_received(from: Account, token_id: TokenId, data: Bytes) -> Bytes
```

`transfer_and_call` moves the tokens to the contract `to`, then calls its `on_token_received` (`on_nft_received` for
KS-721) with the funds attached to it, and returns the receiver's answer; when the receiver refuses or fails, nothing
moves. A receiver checks that `caller()` is a token it accepts, since anyone can call it directly, and answers with
`standard::reply`. A token built on the SDK, whole:

```rust
//! A KS-20 token on the contract SDK's reference implementation: `init` mints the whole supply to the deployer, and
//! holders can burn their tokens. Built on the KS-20 token page of the playground, which has the `standards` feature.
//!
//! ```abi
//! init(name: String, symbol: String, decimals: u8, supply: u128)   // the deployer gets the supply
//! view standards() -> Vec<String>                                  // ["KS-20", "KS-20-burnable"]
//! view metadata() -> (String, String, u8)                          // (name, symbol, decimals)
//! view capabilities() -> Vec<String>
//! view total_supply() -> u128
//! view balance_of(account: Account) -> u128
//! view allowance(owner: Account, spender: Account) -> u128         // 0 once it expired
//! view allowance_info(owner: Account, spender: Account) -> (u128, Option<u64>)
//! call transfer(to: Account, amount: u128)
//! call transfer_from(from: Account, to: Account, amount: u128)     // with the allowance from gave the caller
//! call approve(spender: Account, amount: u128, expires_at: Option<u64>)   // sets the allowance; 0 revokes
//! payable call transfer_and_call(to: ContractId, amount: u128, data: Bytes) -> Bytes
//! call burn(amount: u128)
//! call burn_from(from: Account, amount: u128)                      // with the allowance from gave the caller
//! event standards(ids: Vec<String>)
//! event metadata(name: String, symbol: String, decimals: u8)
//! event transfer(from: Account, to: Account, amount: u128)
//! event approve(owner: Account, spender: Account, amount: u128, expires_at: Option<u64>)
//! ```
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

use alloc::string::String;
use ksana_contract_sdk::{self as sdk, ks20::Ks20, standard, CallResult};

sdk::entry!(dispatch);

const TOKEN: Ks20 = Ks20::new().burnable();

pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    if method == "init" {
        standard::no_funds()?;
        let (name, symbol, decimals, supply): (String, String, u8, u128) = sdk::decode(args)?;
        TOKEN.init(&name, &symbol, decimals)?;
        TOKEN.mint(&sdk::caller(), supply)?;
        return sdk::ok(&());
    }
    TOKEN
        .dispatch(method, args)
        .unwrap_or_else(|| sdk::revert("unknown method"))
}
```

`Ks20::new()` takes `.burnable()` and `.mintable()` (its `init` then calls `init_minter` after `init`), and
`.with_policy(rules)` last, for rules of its own in a `Policy`; a method the token answers itself before calling
`TOKEN.dispatch` replaces the implementation's. `Ks721::new()` takes `.burnable()` and `.enumerable()`; the collection
writes its own `init` (calling `init(name, symbol)`) and its minting method (calling `mint(to, id, uri)`). The
implementations keep their state under the storage prefixes `KS20:` and `KS721:`, so a contract keeps its own state
under other keys. `conformance::ks20(setup)` and `conformance::ks721(setup)` test a token or a collection against the
standard on the mock host.

```rust
// ksana_contract_sdk::standard
/// The zero account: the `from` of a mint and the `to` of a burn.
pub const ZERO_ACCOUNT: [u8; 32] = [0u8; 32];
pub const STANDARDS: &str = "standards";
pub const METADATA: &str = "metadata";
pub const CAPABILITIES_VIEW: &str = "capabilities";
pub const ERR_UNKNOWN_METHOD: &[u8] = b"unknown method";
pub const ERR_UNEXPECTED_FUNDS: &[u8] = b"unexpected attached funds";
pub const ERR_ZERO_ACCOUNT: &[u8] = b"zero account";
pub const ERR_SELF_RECIPIENT: &[u8] = b"recipient is this contract";
pub const ERR_NOT_THE_MINTER: &[u8] = b"not the minter";
pub const ERR_MINTING_ENDED: &[u8] = b"minting has ended";
pub const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
pub const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
pub const ERR_NAME: &[u8] = b"name must be 1 to 64 bytes";
pub const ERR_SYMBOL: &[u8] = b"symbol must be 1 to 16 bytes";
pub const ERR_MALFORMED_RETURN: &[u8] = b"malformed return value";
pub const ERR_MISSING_INDEX: &[u8] = b"missing index entry";
pub const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";
pub const REJECTED: &str = "rejected: ";
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_SYMBOL_BYTES: usize = 16;
pub const CAP_MINT: &str = "mint";
pub const CAP_FREEZE: &str = "freeze";
pub const CAP_ALLOWLIST: &str = "allowlist";
pub const CAP_PAUSE: &str = "pause";
pub const CAP_CLAWBACK: &str = "clawback";
pub const CAP_FEE: &str = "fee";
pub const CAP_UPGRADE: &str = "upgrade";
/// The order a `capabilities()` view lists them in.
pub const CAPABILITIES: [&str; 7] = [CAP_MINT, CAP_FREEZE, CAP_ALLOWLIST, CAP_PAUSE, CAP_CLAWBACK, CAP_FEE, CAP_UPGRADE];
/// `rejected: <reason>`: a refusal by the contract's own rules.
pub fn rejected(reason: &str) -> Vec<u8>;
pub fn is_rejection(reason: &[u8]) -> bool;
/// Reverts with `unexpected attached funds` when funds are attached.
pub fn no_funds() -> Result<(), Vec<u8>>;
/// Reverts unless `to` is neither the zero account nor this contract.
pub fn check_recipient(to: &[u8; 32]) -> Result<(), Vec<u8>>;
pub fn check_name(name: &str) -> Result<(), Vec<u8>>;
pub fn check_symbol(symbol: &str) -> Result<(), Vec<u8>>;
/// The answer of `on_token_received` or `on_nft_received`: `value`'s Borsh bytes as `Bytes`.
pub fn reply<T: BorshSerialize + ?Sized>(value: &T) -> CallResult;
/// Whether an allowance or operator expiring at `expires_at` has expired at this block's timestamp.
pub fn expired(expires_at: Option<u64>) -> bool;
/// The standards `contract` declares, `None` when it answers no list (a caught call).
pub fn standards_of(contract: &[u8; 32]) -> Option<Vec<String>>;
pub fn declares(contract: &[u8; 32], id: &str) -> bool;
/// A fungible asset of either kind in one argument: Borsh tag 0 and a token contract's ID, or tag 1 and a built-in
/// asset's ID (KSA: 1 and 32 zero bytes).
pub enum AssetRef { Token([u8; 32]), Asset([u8; 32]) }
impl AssetRef {
    pub fn id(&self) -> [u8; 32];
    /// The token's `balance_of`, or the built-in balance.
    pub fn balance_of(&self, account: &[u8; 32]) -> Result<u128, Vec<u8>>;
    /// Pays from this contract's balance: the token's `transfer`, or `asset_transfer`.
    pub fn pay(&self, to: &[u8; 32], amount: u128);
}
```

```rust
// ksana_contract_sdk::ks20
pub const ID: &str = "KS-20";
pub const BURNABLE: &str = "KS-20-burnable";
pub const MINTABLE: &str = "KS-20-mintable";
pub const ON_RECEIVED: &str = "on_token_received";
pub const MAX_DECIMALS: u8 = 18;
/// An allowance that never goes down.
pub const UNLIMITED: u128 = u128::MAX;
pub const ERR_ZERO_AMOUNT: &[u8] = b"zero amount";
pub const ERR_INSUFFICIENT_BALANCE: &[u8] = b"insufficient balance";
pub const ERR_INSUFFICIENT_ALLOWANCE: &[u8] = b"insufficient allowance";
pub const ERR_ALLOWANCE_EXPIRED: &[u8] = b"allowance expired";
pub const ERR_SPENDER_IS_CALLER: &[u8] = b"spender is the caller";
pub const ERR_DECIMALS: &[u8] = b"decimals must be at most 18";
pub const ERR_CAP_EXCEEDED: &[u8] = b"cap exceeded";
pub const ERR_SUPPLY_OVERFLOW: &[u8] = b"supply overflow";
pub const REASON_FEE: &str = "the fee takes the whole amount";
/// A KS-20 token to call from another contract: `Token(id).balance_of(&account)?`.
pub struct Token(pub [u8; 32]);
impl Token {
    pub fn standards(&self) -> Result<Vec<String>, Vec<u8>>;
    pub fn metadata(&self) -> Result<(String, String, u8), Vec<u8>>;
    pub fn total_supply(&self) -> Result<u128, Vec<u8>>;
    pub fn balance_of(&self, account: &[u8; 32]) -> Result<u128, Vec<u8>>;
    pub fn allowance(&self, owner: &[u8; 32], spender: &[u8; 32]) -> Result<u128, Vec<u8>>;
    /// Sends this contract's tokens.
    pub fn transfer(&self, to: &[u8; 32], amount: u128);
    /// Takes tokens with the allowance `from` gave this contract.
    pub fn transfer_from(&self, from: &[u8; 32], to: &[u8; 32], amount: u128);
    pub fn approve(&self, spender: &[u8; 32], amount: u128, expires_at: Option<u64>);
    pub fn transfer_and_call(&self, to: &[u8; 32], amount: u128, data: &[u8], funds: &[Funds]) -> Result<Vec<u8>, Vec<u8>>;
    /// `transfer` that returns the token's refusal instead of failing the operation.
    pub fn try_transfer(&self, to: &[u8; 32], amount: u128) -> Result<(), CallError>;
    pub fn try_transfer_from(&self, from: &[u8; 32], to: &[u8; 32], amount: u128) -> Result<(), CallError>;
}
/// What `on_token_received` got: `token` is its caller, and `from` and `amount` are true only when that is a token
/// this contract accepts.
pub struct Received { pub token: [u8; 32], pub from: [u8; 32], pub amount: u128, pub data: Vec<u8> }
/// Decodes the arguments of `on_token_received`.
pub fn received(args: &[u8]) -> Result<Received, Vec<u8>>;
pub enum Event {
    Standards(Vec<String>),
    Metadata { name: String, symbol: String, decimals: u8 },
    Transfer { from: [u8; 32], to: [u8; 32], amount: u128 },
    Approve { owner: [u8; 32], spender: [u8; 32], amount: u128, expires_at: Option<u64> },
    Minter { previous: Option<[u8; 32]>, minter: Option<[u8; 32]> },
}
impl Event {
    pub fn parse(data: &[u8]) -> Option<Self>;
    pub fn encode(&self) -> Vec<u8>;
    pub fn emit(&self);
}
/// A token's own rules; every method has a default of none.
pub trait Policy {
    /// Before every change of a balance (a mint is from the zero account, a burn to it); refuse with `rejected(…)`.
    fn check(&self, from: &[u8; 32], to: &[u8; 32], amount: u128) -> Result<(), Vec<u8>>;
    /// `Some((account, fee))` takes `fee` out of a transfer's amount for `account`.
    fn fee(&self, from: &[u8; 32], to: &[u8; 32], amount: u128) -> Result<Option<([u8; 32], u128)>, Vec<u8>>;
    /// The capabilities these rules give someone now, such as `freeze`.
    fn capabilities(&self) -> Result<Vec<&'static str>, Vec<u8>>;
}
pub struct NoPolicy;
/// The reference implementation, as a `const` of the token: `const TOKEN: Ks20 = Ks20::new().burnable();`.
pub struct Ks20<P = NoPolicy> { .. }
impl Ks20<NoPolicy> {
    pub const fn new() -> Self;
    pub const fn burnable(self) -> Self;
    pub const fn mintable(self) -> Self;
    pub const fn with_policy<Q: Policy>(self, policy: Q) -> Ks20<Q>;
}
impl<P: Policy> Ks20<P> {
    /// Answers every standard method the token has; `None` for the others, `init` included.
    pub fn dispatch(&self, method: &str, args: &[u8]) -> Option<CallResult>;
    /// Records the metadata and emits the declaration, in `init` before any mint.
    pub fn init(&self, name: &str, symbol: &str, decimals: u8) -> Result<(), Vec<u8>>;
    /// `KS-20-mintable`: the first minter and the cap, in `init` after `init`.
    pub fn init_minter(&self, minter: Option<[u8; 32]>, cap: Option<u128>) -> Result<(), Vec<u8>>;
    pub fn policy(&self) -> &P;
    pub fn standards(&self) -> Vec<String>;
    pub fn metadata(&self) -> Result<(String, String, u8), Vec<u8>>;
    pub fn set_metadata(&self, name: &str, symbol: &str) -> Result<(), Vec<u8>>;
    pub fn capabilities(&self) -> Result<Vec<String>, Vec<u8>>;
    pub fn total_supply(&self) -> Result<u128, Vec<u8>>;
    pub fn balance_of(&self, account: &[u8; 32]) -> Result<u128, Vec<u8>>;
    pub fn allowance(&self, owner: &[u8; 32], spender: &[u8; 32]) -> Result<u128, Vec<u8>>;
    pub fn allowance_info(&self, owner: &[u8; 32], spender: &[u8; 32]) -> Result<(u128, Option<u64>), Vec<u8>>;
    pub fn minter(&self) -> Result<Option<[u8; 32]>, Vec<u8>>;
    pub fn cap(&self) -> Result<Option<u128>, Vec<u8>>;
    /// Sets the minter (the method checks the caller first); `None` ends minting.
    pub fn set_minter(&self, minter: Option<[u8; 32]>) -> Result<(), Vec<u8>>;
    /// Moves `amount` from `from` to `to` by the token's rules; returns what `to` received.
    pub fn transfer(&self, from: &[u8; 32], to: &[u8; 32], amount: u128) -> Result<u128, Vec<u8>>;
    pub fn transfer_from(&self, spender: &[u8; 32], from: &[u8; 32], to: &[u8; 32], amount: u128) -> Result<u128, Vec<u8>>;
    pub fn transfer_and_call(&self, from: &[u8; 32], to: &[u8; 32], amount: u128, data: &[u8]) -> Result<Vec<u8>, Vec<u8>>;
    pub fn approve(&self, owner: &[u8; 32], spender: &[u8; 32], amount: u128, expires_at: Option<u64>) -> Result<(), Vec<u8>>;
    pub fn spend_allowance(&self, owner: &[u8; 32], spender: &[u8; 32], amount: u128) -> Result<(), Vec<u8>>;
    /// Creates tokens for `to`, up to the cap; the token's own minting method checks the caller first.
    pub fn mint(&self, to: &[u8; 32], amount: u128) -> Result<(), Vec<u8>>;
    pub fn burn(&self, from: &[u8; 32], amount: u128) -> Result<(), Vec<u8>>;
    pub fn burn_from(&self, spender: &[u8; 32], from: &[u8; 32], amount: u128) -> Result<(), Vec<u8>>;
}
```

```rust
// ksana_contract_sdk::ks721
pub const ID: &str = "KS-721";
pub const BURNABLE: &str = "KS-721-burnable";
pub const ENUMERABLE: &str = "KS-721-enumerable";
pub const ON_RECEIVED: &str = "on_nft_received";
pub const MAX_URI_BYTES: usize = 2_048;
pub const MAX_PAGE: u32 = 100;
pub type TokenId = [u8; 32];
pub const ERR_UNKNOWN_TOKEN: &[u8] = b"unknown token";
pub const ERR_NOT_THE_OWNER: &[u8] = b"not the owner";
pub const ERR_NOT_AUTHORIZED: &[u8] = b"not authorized";
pub const ERR_WRONG_OWNER: &[u8] = b"wrong owner";
pub const ERR_ALREADY_THE_OWNER: &[u8] = b"already the owner";
pub const ERR_SPENDER_IS_OWNER: &[u8] = b"spender is the owner";
pub const ERR_OPERATOR_IS_CALLER: &[u8] = b"operator is the caller";
pub const ERR_TOKEN_EXISTS: &[u8] = b"token exists";
pub const ERR_TOKEN_BURNED: &[u8] = b"token was burned";
pub const ERR_URI_TOO_LONG: &[u8] = b"uri too long";
/// The ID of serial number `n`: `n` big-endian in the last 16 bytes.
pub fn id(n: u128) -> TokenId;
/// The serial number of an ID whose first 16 bytes are zero.
pub fn serial(token_id: &TokenId) -> Option<u128>;
/// A KS-721 collection to call from another contract.
pub struct Collection(pub [u8; 32]);
impl Collection {
    pub fn standards(&self) -> Result<Vec<String>, Vec<u8>>;
    pub fn metadata(&self) -> Result<(String, String), Vec<u8>>;
    pub fn owner_of(&self, token_id: &TokenId) -> Result<Option<[u8; 32]>, Vec<u8>>;
    pub fn balance_of(&self, owner: &[u8; 32]) -> Result<u64, Vec<u8>>;
    pub fn token_uri(&self, token_id: &TokenId) -> Result<Option<String>, Vec<u8>>;
    pub fn transfer(&self, to: &[u8; 32], token_id: &TokenId);
    pub fn transfer_from(&self, from: &[u8; 32], to: &[u8; 32], token_id: &TokenId);
    pub fn approve(&self, spender: Option<&[u8; 32]>, token_id: &TokenId);
    pub fn set_operator(&self, operator: &[u8; 32], approved: bool, expires_at: Option<u64>);
    pub fn transfer_and_call(&self, to: &[u8; 32], token_id: &TokenId, data: &[u8], funds: &[Funds]) -> Result<Vec<u8>, Vec<u8>>;
    pub fn try_transfer(&self, to: &[u8; 32], token_id: &TokenId) -> Result<(), CallError>;
    pub fn try_transfer_from(&self, from: &[u8; 32], to: &[u8; 32], token_id: &TokenId) -> Result<(), CallError>;
}
/// What `on_nft_received` got; `collection` is its caller.
pub struct Received { pub collection: [u8; 32], pub from: [u8; 32], pub token_id: TokenId, pub data: Vec<u8> }
pub fn received(args: &[u8]) -> Result<Received, Vec<u8>>;
pub enum Event {
    Standards(Vec<String>),
    Metadata { name: String, symbol: String },
    Transfer { from: [u8; 32], to: [u8; 32], token_id: TokenId },
    Approve { owner: [u8; 32], spender: Option<[u8; 32]>, token_id: TokenId },
    Operator { owner: [u8; 32], operator: [u8; 32], approved: bool, expires_at: Option<u64> },
    Uri(Option<TokenId>),
}
impl Event {
    pub fn parse(data: &[u8]) -> Option<Self>;
    pub fn encode(&self) -> Vec<u8>;
    pub fn emit(&self);
}
/// A collection's own rules; every method has a default of none.
pub trait Policy {
    /// Before every change of owner (a mint is from the zero account, a burn to it).
    fn check(&self, from: &[u8; 32], to: &[u8; 32], token_id: &TokenId) -> Result<(), Vec<u8>>;
    fn capabilities(&self) -> Result<Vec<&'static str>, Vec<u8>>;
}
pub struct NoPolicy;
/// The reference implementation: `const NFT: Ks721 = Ks721::new().burnable().enumerable();`.
pub struct Ks721<P = NoPolicy> { .. }
impl Ks721<NoPolicy> {
    pub const fn new() -> Self;
    pub const fn burnable(self) -> Self;
    pub const fn enumerable(self) -> Self;
    pub const fn with_policy<Q: Policy>(self, policy: Q) -> Ks721<Q>;
}
impl<P: Policy> Ks721<P> {
    /// Answers every standard method the collection has; `None` for the others, `init` and minting included.
    pub fn dispatch(&self, method: &str, args: &[u8]) -> Option<CallResult>;
    pub fn init(&self, name: &str, symbol: &str) -> Result<(), Vec<u8>>;
    pub fn policy(&self) -> &P;
    pub fn standards(&self) -> Vec<String>;
    pub fn metadata(&self) -> Result<(String, String), Vec<u8>>;
    pub fn set_metadata(&self, name: &str, symbol: &str) -> Result<(), Vec<u8>>;
    pub fn capabilities(&self) -> Result<Vec<String>, Vec<u8>>;
    pub fn owner_of(&self, token_id: &TokenId) -> Result<Option<[u8; 32]>, Vec<u8>>;
    pub fn balance_of(&self, owner: &[u8; 32]) -> Result<u64, Vec<u8>>;
    pub fn token_uri(&self, token_id: &TokenId) -> Result<Option<String>, Vec<u8>>;
    pub fn approved(&self, token_id: &TokenId) -> Result<Option<[u8; 32]>, Vec<u8>>;
    pub fn operator_info(&self, owner: &[u8; 32], operator: &[u8; 32]) -> Result<(bool, Option<u64>), Vec<u8>>;
    pub fn is_operator(&self, owner: &[u8; 32], operator: &[u8; 32]) -> Result<bool, Vec<u8>>;
    pub fn is_burned(&self, token_id: &TokenId) -> bool;
    /// Whether `account` may move `owner`'s token: the owner, the approved account or an operator.
    pub fn may_move(&self, account: &[u8; 32], owner: &[u8; 32], token_id: &TokenId) -> Result<bool, Vec<u8>>;
    pub fn transfer(&self, caller: &[u8; 32], to: &[u8; 32], token_id: &TokenId) -> Result<(), Vec<u8>>;
    pub fn transfer_from(&self, caller: &[u8; 32], from: &[u8; 32], to: &[u8; 32], token_id: &TokenId) -> Result<(), Vec<u8>>;
    pub fn transfer_and_call(&self, caller: &[u8; 32], to: &[u8; 32], token_id: &TokenId, data: &[u8]) -> Result<Vec<u8>, Vec<u8>>;
    pub fn approve(&self, caller: &[u8; 32], spender: Option<&[u8; 32]>, token_id: &TokenId) -> Result<(), Vec<u8>>;
    pub fn set_operator(&self, owner: &[u8; 32], operator: &[u8; 32], approved: bool, expires_at: Option<u64>) -> Result<(), Vec<u8>>;
    /// Mints a new ID to `to`; an ID minted before, even burned, is refused.
    pub fn mint(&self, to: &[u8; 32], token_id: &TokenId, uri: Option<&str>) -> Result<(), Vec<u8>>;
    pub fn burn(&self, token_id: &TokenId) -> Result<(), Vec<u8>>;
    pub fn set_token_uri(&self, token_id: &TokenId, uri: Option<&str>) -> Result<(), Vec<u8>>;
    pub fn total_supply(&self) -> Result<u64, Vec<u8>>;
    pub fn tokens(&self, start: u64, limit: u32) -> Result<Vec<TokenId>, Vec<u8>>;
    pub fn tokens_of(&self, owner: &[u8; 32], start: u64, limit: u32) -> Result<Vec<TokenId>, Vec<u8>>;
}
```

```rust
// ksana_contract_sdk::conformance
/// Runs the KS-20 checks against the token `setup` deploys on the freshly reset mock host.
pub fn ks20(setup: impl Fn() -> Ks20Subject) -> Outcome;
pub fn ks721(setup: impl Fn() -> Ks721Subject) -> Outcome;
pub type Outcome = Result<(), String>;
/// The token and an account holding at least `MIN_BALANCE` that may send to the kit's accounts.
pub struct Ks20Subject { pub token: [u8; 32], pub holder: [u8; 32] }
/// The collection, an account owning at least `MIN_TOKENS`, and those tokens.
pub struct Ks721Subject { pub collection: [u8; 32], pub holder: [u8; 32], pub tokens: Vec<TokenId> }
pub const MIN_BALANCE: u128 = 100;
pub const MIN_TOKENS: usize = 2;
pub const ALICE: [u8; 32] = [0xA1; 32];
pub const BOB: [u8; 32] = [0xB2; 32];
pub const RECEIVER: [u8; 32] = [0xC3; 32];
pub const FUNDS_ASSET: [u8; 32] = [0xF4; 32];
pub const UNKNOWN_TOKEN: TokenId = [0xE5; 32];
```

## Mistakes to avoid

- `storage::read` returns `Result<Option<T>, _>`: handle `None` (`.ok_or_else(|| b"not initialized".to_vec())?` or
  `.unwrap_or(0)`), never `unwrap`.
- Decoding the wrong layout: the arguments of `f(a: A, b: B)` are the Borsh tuple `(A, B)`; one argument is itself, and
  none is empty bytes (`sdk::decode::<()>(args)?`).
- `asset_transfer`, `asset_burn` and `call` fail the whole operation on a short balance, an asset's rules or a callee's
  failure, without the contract's own reason: check first and revert with one, or catch the callee with `try_call`.
- A contract's own balance is `asset_balance(&asset, &sdk::self_id())`, and attached funds are in it before the method
  runs.
- Accounts are 32 bytes: a `tb1p…` address is how wallets write one, never what a contract compares.
- Every later `asset_issue` of a `sub_id` passes exactly the `meta` of the first, or it traps.
- `delegate_call` runs the other code with this contract's storage and balances: only for code you trust.
- `format!` and `to_string()` work but pull `core::fmt` into the module and make it larger, and every byte of the
  module costs gas at deployment and at each call: prefer fixed reason strings.
- `ksana_call` keeps nothing a method writes: a method whose writes matter is a `call` in the ABI block, sent as an
  operation.
