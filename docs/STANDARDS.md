# Ksana Token Standards: KS-20 and KS-721

This document defines the two token standards of Ksana's contracts: KS-20 for fungible tokens and KS-721 for non-fungible tokens (D-120). They are conventions between
contracts, wallets, explorers and indexers, not consensus rules: the VM runs a KS-20 token like any other contract (`docs/ABI.md`), and nothing in a block's validity depends
on them. The reference implementation is the contract SDK's `ks20`, `ks721` and `standard` modules (`crates/ksana-contract-sdk`, feature `standards`), with a conformance kit in its
`conformance` module; the example token `contracts/examples/token` is the reference KS-20 token. Where this document and the reference implementation disagree, the document is
authoritative and the implementation is fixed.

The words **must**, **must not** and **should** are requirements on a contract that declares a standard.

## 1. Conventions

- Arguments and return values are Borsh-encoded, as everywhere in Ksana's contracts: several arguments form a tuple in the order listed, a single argument is just itself, no
  arguments is empty bytes. Every method fails on arguments that do not decode or are not fully consumed.
- An `Account` is 32 bytes: a user's Taproot output key or a contract ID (`docs/ABI.md` §6). A `ContractId` is an account that must be a contract. The **zero account** is
  32 zero bytes: the `from` of a mint and the `to` of a burn in `transfer` events, never a holder.
- `Bytes` is a Borsh `Vec<u8>`; `Option<T>` and `String` are Borsh's (a `String` is UTF-8).
- An event is the Borsh encoding of a tuple whose first element is the event's name: `transfer(from, to, amount)` is `("transfer", from, to, amount)`. The VM records who
  emitted it (`docs/ABI.md` §3), so a contract emits events only in its own name.
- Interfaces are written in the notation of the contract playground's `abi` blocks: `view` methods change nothing, `call` methods may, and `payable` marks the one method of
  each standard that takes attached funds (built-in assets, `docs/ABI.md` §2).
- A revert reason is the UTF-8 text of the method's `Err` (the receipt shows `revert: <reason>`). The reasons in §5.1 are fixed English phrases, so wallets can translate them and
  contracts can compare them after `try_call_contract`.
- Times are block timestamps in seconds (`block_timestamp`, `docs/ABI.md` §3).

## 2. Declaration and discovery

A contract declares the standards it follows itself:

1. Its `init` **must** emit the event `standards(ids: Vec<String>)` and then `metadata(...)` (§3.4, §4.6) before any `transfer` event.
2. Its view `standards() -> Vec<String>` **must** return the same list for the contract's whole life. The first ID is the standard (`"KS-20"` or `"KS-721"`), followed by the IDs
   of the extensions it implements (§3.6, §4.5). A contract **must not** declare both `"KS-20"` and `"KS-721"`.
3. An indexer takes a contract's first `standards` event as its declaration and counts the contract's standard events from there on; events before it do not count.

Ksana's modules export only `ksana_call` and dispatch methods by name inside the contract (`docs/ABI.md` §1), so the export table says nothing about the methods a contract has,
and a registry would need someone to keep it. A declaration in the contract's own events lets a node index tokens from receipts alone, without running the VM (the node's
standards index, `docs/RPC.md`), and the view lets wallets and other contracts ask at any time. Contracts ask with the SDK's `standard::standards_of(contract)` or
`standard::declares(contract, id)`.

A declaration is a claim, not a guarantee: a contract can declare KS-20 and break its rules. What a wallet trusts is the contract's identity: its ID and the hash of its code,
which cannot change unless the contract delegates its code (`delegate_call`). A wallet can mark a contract whose code hash is that of a known build: the contract
playground's compile service builds the same source into the same module on any machine (D-119).

## 3. KS-20: fungible tokens

### 3.1 Interface

```abi
view standards() -> Vec<String>                                         // ["KS-20", extensions...]
view metadata() -> (String, String, u8)                                 // (name, symbol, decimals)
view capabilities() -> Vec<String>                                      // §5.2
view total_supply() -> u128
view balance_of(account: Account) -> u128
view allowance(owner: Account, spender: Account) -> u128                // what spender may take now: 0 once expired
view allowance_info(owner: Account, spender: Account) -> (u128, Option<u64>)   // (amount, expires_at) as set, expired or not
call transfer(to: Account, amount: u128)
call transfer_from(from: Account, to: Account, amount: u128)
call approve(spender: Account, amount: u128, expires_at: Option<u64>)
payable call transfer_and_call(to: ContractId, amount: u128, data: Bytes) -> Bytes   // returns the recipient's answer
event standards(ids: Vec<String>)
event metadata(name: String, symbol: String, decimals: u8)
event transfer(from: Account, to: Account, amount: u128)
event approve(owner: Account, spender: Account, amount: u128, expires_at: Option<u64>)
```

A contract that takes tokens in one step implements:

```abi
payable call on_token_received(from: Account, amount: u128, data: Bytes) -> Bytes
```

`init` is not part of the interface: each token takes the arguments it needs.

### 3.2 Rules

1. **Declaration** (§2): `init` emits `standards` and then `metadata` before any `transfer`.
2. **Metadata**: `decimals` is at most 18 and never changes. `name` is 1 to 64 bytes and `symbol` 1 to 16 bytes; only the lengths and UTF-8 are checked, and wallets clean
   what they show (§7). A token should keep its name and symbol; one that changes them **must** emit `metadata` again, and wallets point the change out.
3. **Every change is an event**: every change of any account's `balance_of` **must** come with a `transfer` event in the same op, for the amount that changed. A mint is a
   transfer from the zero account and a burn a transfer to it, and `total_supply` is the sum of all balances. A token whose balances change without events (rebasing) does not
   follow KS-20; one that pays a yield keeps shares and emits their transfers.
4. **Fees come out of the amount**: a transfer of `amount` lowers the sender's balance by exactly `amount`. A token that charges a fee credits the recipient with
   `amount − fee` and the fee to its fee account, and emits two `transfer` events: from the sender to the recipient for `amount − fee`, then from the sender to the fee
   account for `fee` (to the zero account when the fee is burned). The fee is less than `amount`; a fee that would take all of it refuses the transfer. `on_token_received`
   is given the amount received. A token that can charge a fee reports `fee` in `capabilities()`; one that does not credits the recipient with exactly `amount`.
5. **No zero amounts**: `transfer`, `transfer_from` and `transfer_and_call` of 0 revert with `zero amount` (so does every extension method that moves an amount); `approve` with 0
   revokes.
6. **Recipients**: the recipient is neither the zero account (`zero account`) nor the token contract itself (`recipient is this contract`). A burn goes through the
   `KS-20-burnable` methods.
7. **Attached funds**: every method but `transfer_and_call` reverts with `unexpected attached funds` when funds are attached, so that nothing gets stuck in the token's
   account. A token's `init` should refuse them too.
8. **Allowances** (§3.5): `approve` sets the allowance, it does not add to it; 0 revokes it. `transfer_from` by `from` itself needs no allowance; by anyone else it takes
   `amount` of the allowance `from` gave the caller, which must not have expired (`allowance expired`) and must cover `amount` (`insufficient allowance`). An allowance of
   `u128::MAX` is unlimited and never goes down. `approve` emits `approve`; `transfer_from` emits no `approve`, and `allowance` gives what is left.
9. **A token's own rules**: a transfer the token's own rules refuse (a frozen account, a pause, a limit, an allowlist) reverts with a reason that starts with `rejected: `.
10. **Checks and effects before the call**: `transfer_and_call` moves the tokens and emits its events before it calls the recipient, which may call the token back.

A transfer with too small a balance reverts with `insufficient balance`. A transfer to oneself is a transfer like any other: it emits its event and may charge a fee.

### 3.3 `transfer_and_call`

An account U calls `transfer_and_call(R, amount, data)` on the token T and may attach built-in assets F:

1. The VM moves F from U to T before T runs (`docs/ABI.md` §2).
2. T moves `amount` from U to R by rules 3 to 9 and emits the events.
3. T calls `R.on_token_received(U, received, data)` with `call_contract`, attaching exactly F (`call_funds()`), where `received` is what R was credited (rule 4).
4. R sees `caller()` = T and `call_funds()` = F and acts on `data` (a swap, a deposit, liquidity). It answers `Bytes` whose content it defines (the SDK's
   `standard::reply(&value)` answers the Borsh bytes of `value`, `reply(&())` empty bytes), and T returns that answer unchanged, byte for byte, as its own: a wallet can show
   what the swap paid out. T reverts with `malformed return value` when the answer does not decode as `Bytes`.
5. When R is no contract, has no `on_token_received`, refuses or fails, the whole op fails: the tokens and F stay with U.

A receiving contract **must** check that `caller()` is a token it accepts: anyone can call `on_token_received` directly with made-up `from` and `amount`, and only the token
itself makes them true. It keeps its records per token, so a fake token only credits itself. The attached funds are always real, whoever the caller: the VM moved them. A
contract that does not take tokens reverts on unknown methods (as Ksana's contracts do), so `transfer_and_call` to it fails and the tokens stay with the sender; a plain
`transfer` to a contract that cannot move tokens strands them, which is why wallets suggest `transfer_and_call` when the recipient is a contract (§7).

`transfer_and_call` turns `approve` and a call into one op: a swap of a token pays one fee, and a pool of a token and a built-in asset takes liquidity in one op, the asset
attached to `transfer_and_call` and passed on by the token. v1 defines neither `transfer_from_and_call` nor `approve_and_call`, and no signed permits: a contract can check
BIP340 signatures (`verify_schnorr`), but the wallets sign messages with BIP322, which a contract would have to rebuild.

### 3.4 Events

| Event | Fields | When |
| --- | --- | --- |
| `standards` | `ids: Vec<String>` | in `init`, once |
| `metadata` | `name: String, symbol: String, decimals: u8` | in `init`; again when the name or the symbol changes |
| `transfer` | `from: Account, to: Account, amount: u128` | every change of a balance (rule 3): mints from the zero account, burns to it, a fee as a transfer of its own |
| `approve` | `owner: Account, spender: Account, amount: u128, expires_at: Option<u64>` | every `approve`; `expires_at` is `None` when `amount` is 0 |

A `transfer` event is 92 bytes: the name (4 bytes of length and 8 of text), then 32, 32 and 16. Its layout is the example token's from before KS-20, so readers of that token
keep reading KS-20 tokens.

### 3.5 Allowances

An allowance with `expires_at = None` never expires; otherwise it counts while `block_timestamp() < expires_at`. `allowance` is what the spender may take now (0 once
expired), the one number other contracts need; `allowance_info` gives the amount and the expiry as set, so a wallet can list and warn. `approve` reverts with `zero account`
for the zero account as the spender and with `spender is the caller` for the caller itself.

An allowance lets the spender take up to the amount at any time, so an allowance to a contract that is later broken, or that was a phishing contract from the start, can empty
the account. Wallets therefore approve the amount one op needs, with an expiry (a week, say), and ask separately for an unlimited amount or no expiry; they list every
allowance with what is left and revoke it in one op (`approve(spender, 0, None)`), and name the spender: a known contract by its name and code hash, an unknown one as such.
With `transfer_and_call`, most uses need no allowance at all.

### 3.6 Extensions

**`KS-20-burnable`**

```abi
call burn(amount: u128)                          // burns the caller's tokens
call burn_from(from: Account, amount: u128)      // with the allowance from gave the caller (none needed for from itself)
```

**`KS-20-mintable`**

```abi
view minter() -> Option<Account>                 // None once minting ended for good
view cap() -> Option<u128>                       // the most total_supply can reach; None for no cap
call mint(to: Account, amount: u128)             // the minter only (not the minter / minting has ended), up to the cap (cap exceeded)
call set_minter(minter: Option<Account>)         // the minter only; None ends minting for good
event minter(previous: Option<Account>, minter: Option<Account>)
```

A mintable token emits `minter` in `init`, from `None`, when it sets its first minter (or none) and its cap, and again whenever the minter changes; it reports `mint` in
`capabilities()` while it has a minter. A cap below the supply at `init` is refused.

### 3.7 Tokens written before KS-20

The example token took `approve(spender, amount)` and `init(name, symbol, decimals, supply)` before KS-20. The reference token still takes both: an `approve` of two
arguments is one with `expires_at = None`, and an `init` of four is one with no minter and no cap. Callers written for it keep working, and so does a pool that pulls tokens
with `transfer_from` and pays with `transfer`, as the example AMM does.

## 4. KS-721: non-fungible tokens

### 4.1 Interface

```abi
view standards() -> Vec<String>                                       // ["KS-721", extensions...]
view metadata() -> (String, String)                                   // (name, symbol)
view capabilities() -> Vec<String>                                    // §5.2
view owner_of(token_id: [u8; 32]) -> Option<Account>                  // None for a token that does not exist, or was burned
view balance_of(owner: Account) -> u64                                // how many tokens owner holds
view token_uri(token_id: [u8; 32]) -> Option<String>
view approved(token_id: [u8; 32]) -> Option<Account>
view is_operator(owner: Account, operator: Account) -> bool                    // now: false once expired
view operator_info(owner: Account, operator: Account) -> (bool, Option<u64>)   // (approved, expires_at) as set, expired or not
call transfer(to: Account, token_id: [u8; 32])                        // the owner only
call transfer_from(from: Account, to: Account, token_id: [u8; 32])    // the owner, the approved account or an operator; from must own it now
call approve(spender: Option<Account>, token_id: [u8; 32])            // the owner or an operator; None clears
call set_operator(operator: Account, approved: bool, expires_at: Option<u64>)
payable call transfer_and_call(to: ContractId, token_id: [u8; 32], data: Bytes) -> Bytes   // the owner only; returns the recipient's answer
event standards(ids: Vec<String>)
event metadata(name: String, symbol: String)
event transfer(from: Account, to: Account, token_id: [u8; 32])
event approve(owner: Account, spender: Option<Account>, token_id: [u8; 32])
event operator(owner: Account, operator: Account, approved: bool, expires_at: Option<u64>)
event uri(token_id: Option<[u8; 32]>)
```

A contract that takes tokens in one step implements:

```abi
payable call on_nft_received(from: Account, token_id: [u8; 32], data: Bytes) -> Bytes
```

Minting is the collection's own method, as is `init`: collections number their tokens in different ways (serial numbers, the hash of a name, a BTC transaction ID), so the
standard leaves the method out. A collection that can still mint reports `mint` in `capabilities()`.

### 4.2 Rules

1. **Declaration** (§2): `init` emits `standards` and then `metadata` before any `transfer`.
2. **Every change of owner is an event**: every mint, transfer and burn **must** emit `transfer`, a mint from the zero account and a burn to it, and `balance_of` is the number
   of tokens the account holds.
3. **Approvals end with the owner**: a change of owner clears the token's approved account, without an event of its own (indexers apply the rule to the `transfer` event).
4. **Who moves a token**: `transfer` and `transfer_and_call` are the owner's alone (`not the owner`), so the `from` that `on_nft_received` receives is the account that sent
   the token. `transfer_from` is the owner's, the token's approved account's or an operator's of the owner (`not authorized`), and `from` must own the token now
   (`wrong owner`). `from` protects listings: when an owner moves a listed token to another of its wallets and both made the same market an operator, the market's old
   listing names the old wallet, so it cannot sell the token out of the new one.
5. **Recipients**: neither the zero account (`zero account`) nor the collection itself (`recipient is this contract`), and not the token's owner (`already the owner`).
6. **IDs are minted once**: an ID that a token has (`token exists`) or had before it was burned (`token was burned`) is never minted again.
7. **Attached funds**: every method but `transfer_and_call` reverts with `unexpected attached funds` when funds are attached.
8. **Approvals and operators**: `approve` is the owner's or an operator's (`not authorized`); the approved account is never the zero account (`zero account`) nor the owner
   (`spender is the owner`). An operator moves, approves and burns every token of the owner, present and future, while `block_timestamp() < expires_at` (always for `None`);
   `set_operator` reverts with `zero account` for the zero account and `operator is the caller` for the caller itself, and `approved = false` takes the operator back and drops
   its expiry. Both emit their events.
9. **Unknown tokens**: a method naming a token that does not exist reverts with `unknown token` (the views answer `None`).
10. **A collection's own rules** refuse with a reason that starts with `rejected: `, as KS-20's do.
11. **Checks and effects before the call**: `transfer_and_call` moves the token and emits its event before it calls the recipient.
12. **URIs**: a change of a token's URI after its mint emits `uri(Some(token_id))`; a change of every token's (a new base URI) emits `uri(None)`. Wallets read the URIs again.

`transfer_and_call` works as KS-20's (§3.3): the owner U calls `transfer_and_call(R, token_id, data)` on the collection C, attaching F; C moves the token to R and calls
`R.on_nft_received(U, token_id, data)` with F; R checks that `caller()` is a collection it accepts and answers `Bytes`, which C returns unchanged; when R is no contract,
refuses or fails, the whole op fails and the token stays with U.

### 4.3 Token IDs

A token ID is 32 bytes, as long as an account, an asset ID or a transaction ID, so it can hold a hash: a name service uses the SHA-256 of a name, a collection that follows BTC
transactions their IDs. A serial number n is written big-endian in the last 16 bytes (the SDK's `ks721::id(n)`, and `ks721::serial(id)` back), so storage keys sort by serial
number. Wallets show an ID whose first 16 bytes are zero as `#n` in decimal, any other in shortened hex.

### 4.4 Token URIs and metadata

`token_uri` points to a JSON document in ERC-721's metadata format (`name`, `description`, `image`, `attributes`), by `data:`, `https:`, `ipfs:` or `ar:` URI. A wallet that finds
no JSON there treats the URI as an image or as text. A `data:` URI stored on chain is bound by the storage limit of 64 KiB a value; the reference implementation takes URIs of up
to 2 048 bytes.

Wallets show images with `<img>` only (SVG too: never inline, never as a document that runs scripts), load `https` and `ipfs` resources through a proxy or when the user opens
them, with a size limit, and clean names and descriptions as they clean asset names.

### 4.5 Extensions

**`KS-721-burnable`**

```abi
call burn(token_id: [u8; 32])                    // the owner, the approved account or an operator
```

**`KS-721-enumerable`**

```abi
view total_supply() -> u64                                            // how many tokens exist
view tokens(start: u64, limit: u32) -> Vec<[u8; 32]>                  // every token, a page from position start
view tokens_of(owner: Account, start: u64, limit: u32) -> Vec<[u8; 32]>   // owner's tokens, a page
```

A page holds at most 100 IDs; a larger `limit` is cut to 100, and a `start` past the end gives an empty page. Each list holds every token once; the order is the
implementation's (the reference implementation's moves the last token into a burned or departed token's place). Keeping the lists costs gas at every mint, transfer and burn,
which is why the extension is optional; wallets list a holder's tokens from a node's index without it (`ksana_getNfts`, `docs/RPC.md`).

### 4.6 Events

| Event | Fields | When |
| --- | --- | --- |
| `standards` | `ids: Vec<String>` | in `init`, once |
| `metadata` | `name: String, symbol: String` | in `init`; again when the name or the symbol changes |
| `transfer` | `from: Account, to: Account, token_id: [u8; 32]` | every mint, transfer and burn |
| `approve` | `owner: Account, spender: Option<Account>, token_id: [u8; 32]` | every `approve` (`None` clears) |
| `operator` | `owner: Account, operator: Account, approved: bool, expires_at: Option<u64>` | every `set_operator` |
| `uri` | `token_id: Option<[u8; 32]>` | a token's URI changed after its mint (`None`: every token's) |

## 5. What both standards share

### 5.1 Revert reasons

| Case | Reason |
| --- | --- |
| A method the contract does not have | `unknown method` |
| Arguments that do not decode, or bytes left after them | `malformed arguments` |
| Funds attached to a method that takes none | `unexpected attached funds` |
| An amount of 0 (KS-20) | `zero amount` |
| The zero account as recipient, spender, operator or minter | `zero account` |
| The token or the collection itself as recipient | `recipient is this contract` |
| Too small a balance (KS-20) | `insufficient balance` |
| Too small an allowance (KS-20) | `insufficient allowance` |
| An expired allowance (KS-20) | `allowance expired` |
| `approve` for oneself (KS-20) | `spender is the caller` |
| A token that does not exist (KS-721) | `unknown token` |
| `transfer` or `transfer_and_call` by anyone but the owner (KS-721) | `not the owner` |
| `transfer_from`, `approve` or `burn` by an account that may not (KS-721) | `not authorized` |
| `transfer_from` naming another `from` than the owner (KS-721) | `wrong owner` |
| A transfer to the owner (KS-721) | `already the owner` |
| The owner as the approved account (KS-721) | `spender is the owner` |
| `set_operator` for oneself (KS-721) | `operator is the caller` |
| A mint of an ID a token has, or had (KS-721) | `token exists`, `token was burned` |
| A recipient's answer that is not `Bytes` | `malformed return value` |
| `mint` or `set_minter` by anyone but the minter (`KS-20-mintable`) | `not the minter`, or `minting has ended` once there is none |
| A mint past the cap (`KS-20-mintable`) | `cap exceeded` |
| A refusal by the contract's own rules | `rejected: <reason>` |

When the recipient of `transfer_and_call` refuses, the receipt shows the recipient's reason. The reference implementation also reverts with `name must be 1 to 64 bytes`,
`symbol must be 1 to 16 bytes`, `decimals must be at most 18` and `already initialized` in `init`, `supply overflow` for a mint past `u128::MAX` and `uri too long` for a token
URI of more than 2 048 bytes; a contract's own `init` and limits may use other reasons.

### 5.2 Capabilities

`capabilities() -> Vec<String>` reports what the contract's rules let someone do now, so that a wallet can mark the asset:

| Name | Someone can |
| --- | --- |
| `mint` | create more tokens (a minter is set, the collection can still mint) |
| `freeze` | stop an account's tokens from moving |
| `allowlist` | limit who may hold or receive the tokens |
| `pause` | stop every transfer |
| `clawback` | take tokens from a holder without the holder's approval |
| `fee` | charge a fee on transfers (KS-20 rule 4) |
| `upgrade` | change the contract's code (a contract that delegates its code) |

The list holds each name once, in the order of this table, with names this document does not define after them. A capability that has ended is no longer reported: a token
whose minter gave minting up reports no `mint`. The first four are also the names, in this order, of the issuer capabilities of built-in assets (D-121), which have no
`clawback` and no `fee`, so that a wallet marks both kinds of asset alike; it says that a built-in asset's are the protocol's while a contract's are the contract's own word.

### 5.3 `AssetRef`: either kind of fungible asset

A contract that takes any fungible asset in one argument (a pool, a market) uses the SDK's `standard::AssetRef`:

```abi
enum AssetRef { Token(ContractId), Asset([u8; 32]) }   // a KS-20 token; a built-in asset (KSA is 32 zero bytes)
```

Its Borsh encoding is the variant's index (`Token` 0, `Asset` 1) and the 32-byte ID, the same as the example AMM's `Side`. `AssetRef::balance_of` reads the token's
`balance_of` or the built-in balance, and `AssetRef::pay` pays from the contract's own balance with the token's `transfer` or `asset_transfer`.

### 5.4 Choosing a kind of asset

| Need | Use |
| --- | --- |
| A currency, a points balance, an item counted by quantity | a built-in asset by default: wallets, the explorer and the RPC know it, it can be attached to calls, and a transfer costs 10 000 gas |
| Balances with logic of their own: shares that earn, vote snapshots, vesting, dividends, data per holder, transfers that read or write other contracts | KS-20 |
| Items each unique: collectibles, tickets, names, position receipts | KS-721 |
| Many fungible items from one contract | built-in assets: one contract issues any number of them (`sub_id`, `docs/ABI.md` §3) |

A brand issues one kind. When one has to reach the other's contracts (a KS-20 token in a market that takes built-in assets alone), a wrapper contract locks it one for one and
issues a built-in asset whose name says so (`wXYZ`).

## 6. Reference implementation and conformance kit

The contract SDK's modules (D-120), compiled with its `standards` feature: a contract built on them depends on the SDK with
`ksana-contract-sdk = { workspace = true, features = ["standards"] }`. The feature is off by default, so that a contract not built on them compiles to the same module as
before they were added.

- `standard`: the zero account, the names of the declaration and of the capabilities, the revert reasons, the checks on names and symbols, `reply` (a receiving contract's
  answer), `standards_of` / `declares`, `AssetRef`.
- `ks20`: `Token` to call a KS-20 token from another contract (with `try_` versions), `received` to decode `on_token_received`'s arguments, `Event` to parse and emit the events,
  and `Ks20`, the implementation a token is built on: `Ks20::new()`, then `.burnable()` and `.mintable()` for the extensions and `.with_policy(rules)` for the token's own
  rules (`Policy`: a check before every movement, a fee, the capabilities it reports). `Ks20::dispatch` answers every standard method and `None` for the others; a method the
  token answers before it replaces the implementation's.
- `ks721`: `Collection`, `received`, `Event`, `id` / `serial`, and `Ks721` with `.burnable()`, `.enumerable()` and a `Policy` (a check before every change of owner, which can
  also keep the collection's own records, and the capabilities).

The implementation keeps its state under storage keys that start with `KS20:` or `KS721:`, so a contract built on it keeps its own state under any other key. Amounts and counts
are absent at 0, so each state has one representation.

What it costs in gas, measured in `crates/ksana-execution/tests/contracts.rs` with the gas table of `docs/ABI.md` §4: the example token deploys for about 780 000 gas (28.7 KB of
code, within the CLI's default `--max-gas` of 1 000 000), a `transfer` takes about 84 000, `approve` 68 000, `transfer_from` 99 000 and a `transfer_and_call` to the test receiver
157 000; the test collection (burnable and enumerable) deploys for about 842 000 (33.6 KB), a mint takes 169 000, a transfer 176 000 and a `transfer_and_call` 218 000. A built-in
asset's transfer is 10 000.

`conformance::ks20(setup)` and `conformance::ks721(setup)` check a contract on the SDK's mock host, whatever its own rules: the declaration and the metadata, events that account
for every change, the refusals and their reasons, allowances and operators and their expiry, attached funds, `transfer_and_call` with a receiving contract the kit registers, and
the extensions the contract declares. A contract's native tests call them with a setup that deploys it (the module docs show how): the example token passes the KS-20 kit with
both forms of its `init`, and the test collection `contracts/testing/collection` the KS-721 kit.

## 7. Security notes

- **Impersonation**: wallets clean names and symbols and compare them with KSA and BTC as they do built-in assets' names and symbols, show an unverified token's whole ID, and
  fold away tokens nobody asked for.
- **A contract that claims a standard** can emit false events and answer false balances, but only for itself: the defence is identity (the ID, the code hash), not the balance.
- **Allowances**: exact amounts, expiries, `transfer_and_call` instead of an allowance, a page that lists and revokes them, the spender named (§3.5).
- **Callbacks**: the recipient checks `caller()`; the token moves before it calls; the call depth is at most 8 (`docs/ABI.md` §3).
- **Zero-amount spam and poisoned histories**: transfers of 0 are refused.
- **Stuck funds**: methods that take no funds refuse them; tokens never go to the zero account or the contract itself; wallets suggest `transfer_and_call` when the recipient
  is a contract.
- **Upgradeable tokens** (a contract that delegates its code) change code a wallet cannot see; the reference implementation delegates nothing, and they report `upgrade`.
- **Tokens with rules** may refuse a transfer: a pool or a market treats `rejected: ` as it treats too small a balance.
- **NFT media**: `<img>` only, remote resources through a proxy or on demand, with a size limit.
