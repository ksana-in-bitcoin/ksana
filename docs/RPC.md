# Ksana node and indexer JSON-RPC

The methods of SPEC §11.2 are defined in `crates/ksana-rpc`; the node implements them in `crates/ksana-node/src/rpc.rs`. The server is based on jsonrpsee and supports both
HTTP and WebSocket on the same port (`ksana-node --rpc-addr`, default `127.0.0.1:8545`). Cross-origin calls from browser pages need `--rpc-cors` (`*` or a comma-separated list of origins,
not allowed by default; D-055).

## Conventions

- Byte strings (hashes, accounts, Borsh encodings) are always **lowercase hex strings** without `0x`.
- `u64` / `u128` are decimal strings (JavaScript numbers have only 53 bits of precision); `u8` / `u32` are JSON numbers.
- The JSON form of protocol types such as block headers and entries is the same as in `tests/vectors/` (the serde representation of `ksana-primitives`).
- Block identifier: `"latest"`, a decimal height string, or a 64-hex-char block hash.

## Limits

The node's and the indexer's servers apply the same limits (local policy, D-132, `ksana_rpc::server_config`). jsonrpsee answers what passes them with its own codes:

| Limit | Value | Beyond it |
| --- | --- | --- |
| Calls in one batch | 64 | The batch is refused as a whole: one error object, `-32010`, `id` null, and no call is run |
| Request body (HTTP), or one WebSocket message | 1 MiB | `-32007`, over HTTP with status 413; the body is not read |
| Response body | 10 MiB | `-32008` for a call; for a batch, `-32011` with `id` null |
| Connections at once, HTTP and WebSocket together | 100 | HTTP status 429, "Too many connections. Please try again later." |
| Subscriptions on one WebSocket connection | 16 | `-32006` |

`-32010` and `-32011` answering a batch are not the op errors of the same codes: those answer one `ksana_submitOp` with its own `id`, these a whole batch with `id` null. A client that
batches its calls keeps to 64 of them and sends a batch refused this way again in smaller parts (the web wallet halves it).

## Methods

| Method | Parameters | Result |
| --- | --- | --- |
| `ksana_submitOp` | `op`: the Borsh-encoded VmOp | `{ "op_id" }` |
| `ksana_getOpStatus` | `op_id` | `{ "status", "block", "index", "receipt" }` |
| `ksana_getAccountOps` | `account`, `before`?, `limit`? | `{ "ops": [{ "op_id", "status", "block", "index", "position" }], "next", "at" }` |
| `ksana_getLatestOps` | `before`?, `limit`? | the same as `ksana_getAccountOps` |
| `ksana_getBlock` | block identifier | `{ "hash", "header", "entries", "settlement", "receipts", "accepted_height", "merged" }` |
| `ksana_getAccount` | account (the 32-byte output key of the Taproot address, hex, D-092) | `{ "account", "nonce", "balances", "locked_fees", "at" }` |
| `ksana_getAsset` | asset ID | `{ "asset", "name", "symbol", "decimals", "total_supply", "issuer", "capabilities", "paused", "at" }` |
| `ksana_getAssetStatus` | asset ID, account | `{ "asset", "account", "status", "can_send", "can_receive", "at" }` |
| `ksana_getContract` | contract ID | `{ "contract", "code_hash", "deployer", "standards", "declared", "at" }` |
| `ksana_getAccountAssets` | `account`, `after`?, `limit`? | `{ "assets": [{ "kind", ... }], "next", "at" }` |
| `ksana_getToken` | contract ID | `{ "contract", "standard", "extensions", "name", "symbol", "decimals", "supply", "holders", "declared", "irregular", "at" }` |
| `ksana_getTokenHolders` | contract ID, `after`?, `limit`? | `{ "holders": [{ "account", "balance" }], "next", "at" }` |
| `ksana_getNfts` | `account`, `contract`?, `after`?, `limit`? | `{ "nfts": [{ "contract", "token_id" }], "next", "at" }` |
| `ksana_getApprovals` | `account`, `after`?, `limit`? | `{ "approvals": [{ "kind", ... }], "next", "at" }` |
| `ksana_call` | `{ "contract", "method", "args", "funds"?, "caller"?, "max_gas"? }` | receipt |
| `ksana_getSettlement` | block identifier | `{ "block", "txid", "txid_rpc", "commit_txid", "commit_txid_rpc", "txs", "carrier_account", "carrier", "status", "confirmed", "settled", "btc_final" }` |
| `ksana_estimateFee` | none | `{ "feerate_sat_vb", "path_b": { "amount", "per_kib" }, "at" }` |
| `ksana_getChainInfo` | none | `{ "chain_id", "tip", "daa_score", "protocol_version", "next_upgrade", "max_known_version", "finalized" }` |
| `ksana_subscribeBlocks` | none (WebSocket) | pushes `{ "height", "hash", "entries", "timestamp" }` |

- `ksana_submitOp`: the node pre-checks the op by the consensus rules (version, chain ID, length, signature, nonce not below the account nonce) and decides by local policy (minimum sponsored fee, capacity)
  whether to accept it. When this node produces blocks, the call returns only after the new op has entered the block template: the next block produced afterwards is guaranteed to have considered it.
  - Only path B (`FeeMode::Token`) is accepted: the KSA balance must cover the sponsored fees of the queued ops. Path A has been deleted (D-078); users who pay with BTC first top up to get KSA.
  - Path C is submitted through BTC native transactions (§7.4) and does not go through this method; submitting a path C VmOp returns `-32010`.
- The `status` of `ksana_getOpStatus`: `pending` (in the entry pool), `fast_confirmed` (included and executed successfully), `failed` (included but execution failed;
  the fee is charged anyway), `btc_final` (executed successfully, and the main-chain block that accepted the containing block is BTC-final: on the finalized chain determined from the BTC chain data, where its data and the data of the blocks it
  merges have all received `BTC_FINALITY_CONFS` confirmations on BTC, at or below the BTC-final tip of `ksana_getChainInfo`, D-038, D-063, D-082, D-114), `skipped` (included only by blocks merged by the main chain, and could not be executed under the state at merge time -- duplicate inclusion, a conflict on the same nonce, insufficient balance, etc. -- so it was not executed
  and no fee was charged; while still valid it returns to the entry pool and may be included again later, D-063). `block` is the block that included it (`hash`) and the height of the main-chain block that accepted that block (`height`:
  the ops of a merged block take effect at the height of the main-chain block that merges it). When the same op is included by several parallel blocks, the position where it was executed is given. An unknown op returns error `-32004`.
- `ksana_getAccountOps` / `ksana_getLatestOps` (D-111): the operations on the main chain, newest first, a page at a time: those of one account (`account`, hex as in
  `ksana_getAccount`: the ops it signed, the asset transfers to it, the calls to it as a contract, and the ops whose KS-20 or KS-721 events the standards index applied, listed under the
  contract that emitted them and under every account the events name, D-120, and the ops that changed built-in assets, listed under every account their `asset_changes` name
  and under each asset ID but KSA's, so that an asset's ID lists its history, D-122; a deployment is listed under its signer and the accounts its `init`'s standard events and
  asset changes name, never under the contract it deploys), or those of the whole chain. Path B and path C (forced-inclusion) entries are listed, unlock entries are not.
  - `position`: an op's place on the main chain, the string `"height:slot:entry"` in decimal: the height of the main-chain block that accepted the block including it, that block's place in the
    main-chain block's acceptance order (the blocks it merges first, in the order of `merged` in `ksana_getBlock`, then the main-chain block itself) and the entry's index in the block. Positions
    sort in the order the main chain executes the entries, and a listing runs from the highest down.
  - `before`: a position; the page lists the ops below it (a page's `next` continues the listing); omitted or `null` for the newest. `limit`: the most ops in the page, 1 to 500, 50 when omitted or
    `null`. A malformed account or position, or a `limit` out of range, returns `-32602`.
  - `ops`: each op once, where `ksana_getOpStatus` places it (an op included by several blocks is listed where it was executed, and as `skipped` only when no block executed it). `status`, `block` and
    `index` are those of `ksana_getOpStatus` (`fast_confirmed`, `btc_final`, `failed` or `skipped`, never `pending`); `block.height` is the position's height.
  - `next`: where the next page starts, `null` once the listing reached the genesis block. A page can hold fewer than `limit` ops, even none, while `next` is not `null` (a page passes over at most
    1 024 entries of ops listed elsewhere): follow `next` until it is `null`. `at`: the main-chain tip the page was read at (`{ "height", "hash" }`). Pages read at different tips join up
    without gaps or repeats as long as the main chain at and below the height of `next` did not change in between; after a reorg that deep, list again from the newest.
  - The node keeps the listings next to its main chain in its block store (local, no consensus data). A data directory from an earlier version gets them built from the stored blocks at the node's
    first start (logged); when that fails, both methods return `-32601` until a later start builds them, the same answer as from a node before D-111, which does not have them. Any other
    error (`-32603`) concerns that call only.
- `ksana_getBlock` (D-063): a height in the block identifier refers to a block on the main chain (the virtual selected chain); a hash can be any block in the DAG. `settlement`: the settlement
  transactions carried by the block, `{ "commit", "reveal" }` (the commit and reveal transactions, hex of the Bitcoin network serialization, D-081); `null` for a block that carries no settlement transactions.
  `accepted_height`: the height of the main-chain block that accepted it (for a main-chain block its own height, for a merged block the height of the main-chain block that merges it; for a block not yet accepted by the main chain
  `null`). `merged`: the blocks it merges (the merge set without the selected parent, in consensus order; when it is executed as a main-chain block, the entries of these blocks are first executed leniently in this order;
  empty for a block that is not on the main chain and has been pruned from the node's memory, D-097). `receipts` are the execution results from when it was accepted by the main chain
  (for a merged block the results at merge time; entries whose `error` starts with "skipped" were not executed); empty for a block not yet accepted.
- `ksana_getAccount`: `balances` are the available balances per asset (the asset ID of KSA is 64 `0` characters); `locked_fees` is the account's **not yet unlocked**
  income `[{ "block_hash", "amount" }]` (§8.5, D-074, D-079, D-089): for each block the account produced after the unlock cursor (in acceptance order), the path B sponsored fees plus the producer's share of the block reward
  to be issued at unlock time (KSA base units, estimated with the `KSA_MAX_SUPPLY` cap applied); when querying the block share account, the share it will receive from each block;
  blocks with an amount of 0 are not listed. Once unlocked, a block is no longer listed and its amount goes into
  `balances`. `at` is the block the query is based on.
- `ksana_getAsset` (D-074, D-121): the asset's name, symbol, precision (`decimals`, JSON number), total supply (`total_supply`, decimal string, base units) and issuing contract
  (`issuer`, 64 `0` characters for KSA). The total supply of KSA = genesis allocation + block rewards already issued (issued at unlock time, D-079) − burns, not exceeding `KSA_MAX_SUPPLY`.
  `capabilities` lists the capabilities its issuing contract still holds, by name, in the order `mint`, `freeze`, `allowlist`, `pause` (ABI §3): empty for KSA, and for an
  asset whose supply is fixed and whose movements no one can stop; capabilities are only ever given up. `paused` is whether the issuing contract paused the asset, when
  no one but the issuing contract can send it. A nonexistent asset returns `-32004`. `at` is the block the query is based on.
- `ksana_getAssetStatus` (D-121): an account's standing in an asset. `status` lists the account's status flags for the asset as recorded, by name, in the order
  `frozen`, `allowed`; a flag counts only while the asset has the matching capability. `can_send` is whether the asset's rules let the account send or burn it now
  (not frozen, allowed when the asset has an allowlist, and the asset not paused, unless the account is the issuing contract), its balance aside; `can_receive` whether
  they let it receive the asset now. A wallet can check both before an `AssetTransfer` rather than pay the fee of an op the rules refuse. A nonexistent asset returns
  `-32004`. `at` is the block the query is based on.
- The standards index (D-120, `docs/STANDARDS.md`): `ksana_getContract`, `ksana_getAccountAssets`, `ksana_getToken`, `ksana_getTokenHolders`, `ksana_getNfts` and `ksana_getApprovals`
  answer from the node's index of the KS-20 and KS-721 events in the receipts of the ops the main chain executed (local, no consensus data). The node keeps it next to the operation listings
  of `ksana_getAccountOps`, in the same writes, and builds both from the stored blocks at the first start of a data directory from an earlier version; while they are not built, these
  methods return `-32601` too.
  - What counts: a contract's first `standards` event is its declaration (STANDARDS §2), and from there on the index applies the events of the standard the declaration names first, KS-20
    or KS-721 (a declaration naming both is followed as neither), each only when its bytes are exactly one of the standard's events. An event of the standard that cannot be applied as the
    standard has it (a transfer of more than the sender holds by the token's own events, of an NFT its `from` does not own, a mint of an ID that existed, a second declaration, metadata
    out of bounds or changing the decimals) is passed over and marks the contract `irregular`.
  - The index answers what an account holds, the contract how much: a wallet lists from the index and reads `balance_of`, `allowance` or `owner_of` through `ksana_call` before acting on
    them, and a contract whose answers differ from the index's does not keep its standard's rules. An allowance is listed as its latest `approve` event set it: `transfer_from` emits no
    `approve`, so `amount` stays what was approved while the token's `allowance` answers what is left. Approvals stay listed once they expire (`expires_at`, a block timestamp in seconds,
    decimal string; `null` for none) until an event sets them again.
  - `ksana_getContract`: any contract (an ID that is no contract returns `-32004`): the SHA-256 of its code (`code_hash`), the account that deployed it (`deployer`), the IDs of its
    declaration (`standards`, the standard first; empty without a declaration, or for one that is not 1 to 16 IDs of 1 to 64 bytes) and the `position` of the op whose events hold it
    (`declared`, as in `ksana_getAccountOps`; `null` without one). A contract that keeps its standard declares itself in its `init` (`docs/STANDARDS.md` §2), so `declared`
    is its deployment; since a deployment is never listed under the contract it deploys, `ksana_getLatestOps` with `before` one entry past `declared` and `limit` 1 returns it.
  - `ksana_getToken`: a contract whose declaration names KS-20 or KS-721 first (`-32004` otherwise): `standard`, the declared `extensions`, `name`, `symbol` and `decimals` (JSON number)
    of the latest `metadata` event the index applied (`null` before one; `decimals` is always `null` for a collection), `supply` (the sum of the balances, or the number of the
    collection's NFTs in existence) and `holders` (the number of accounts holding some), both decimal strings, `declared` and `irregular`.
  - `ksana_getAccountAssets`: an account's built-in assets by asset ID (`{ "kind": "asset", "asset", "balance" }`, the balances of `ksana_getAccount`), then the tokens and collections it
    holds some of by contract ID (`{ "kind": "token", "contract", "balance" }`, `{ "kind": "nft", "contract", "count" }`); amounts are decimal strings.
  - `ksana_getTokenHolders` (`-32004` as for `ksana_getToken`): a token's or a collection's holders, the largest balance first (for a collection, `balance` is the number of its NFTs held),
    the same amount by account from the highest.
  - `ksana_getNfts`: an account's NFTs by collection and token ID; with `contract`, those of that collection only.
  - `ksana_getApprovals`: the approvals an account gave, by contract, then kind, then spender, operator or token ID: KS-20 allowances (`{ "kind": "allowance", "contract", "spender",
    "amount", "expires_at" }`), KS-721 operators (`{ "kind": "operator", "contract", "operator", "expires_at" }`) and the approved accounts of single NFTs (`{ "kind": "token", "contract",
    "token_id", "spender" }`, cleared when the NFT changes owner, STANDARDS §4.2).
  - Pages: `after` is a page's `next`, the hex of its last item's key (33 bytes for `ksana_getAccountAssets`, 48 for `ksana_getTokenHolders`, 64 for `ksana_getNfts`, 65 for
    `ksana_getApprovals`), and the page lists what follows it; omitted or `null` for the first page. With `contract`, `ksana_getNfts` takes only a cursor in that collection. `limit`: the
    most items in the page, 1 to 500, 50 when omitted or `null`. `next` is `null` once the listing reached its end. `at`: the main-chain tip the page was read at (`{ "height", "hash" }`);
    an item whose place changed between two pages (a holder whose balance changed) can be missed or listed twice. A malformed ID, account or cursor, or a `limit` out of range, returns
    `-32602`.
- `ksana_getChainInfo` (D-076): the main-chain tip (`tip`: `{ "height", "hash" }`), its DAA score (decimal string) and protocol version (block header version),
  the next scheduled upgrade after the tip (`next_upgrade`: `{ "version", "daa_score" }`, `null` when there is none), the highest protocol version supported by this node's software
  (`max_known_version`): when the network's version is higher the node software needs upgrading, and the BTC-final tip (`finalized`: `{ "height", "hash" }`, D-082, D-114):
  the highest block of the finalized chain whose determination was made at a BTC block that has `BTC_FINALITY_CONFS` confirmations (SPEC §6.5 item 7); the main chain passes through it, and only a BTC reorg
  replacing that BTC block, at least `BTC_FINALITY_CONFS` blocks, can take it back; the genesis block when there is none yet. The finalized tip the node follows can be higher, as its determination does not wait. The DAA score accumulates the weight of each block: 1 for a non-empty block; for an empty block (no entries), by the number of consecutive
  empty blocks on the selected chain, doubling block by block from `EMPTY_BLOCK_DIFFICULTY` (12) to `MAX_EMPTY_BLOCK_WEIGHT` (120) (D-084, D-090).
- `ksana_call`: executes one read-only call on the latest state; no fee is charged, the nonce is not changed, nothing goes on-chain; `caller` defaults to 32 zero bytes, `max_gas` defaults to the next block's
  `BLOCK_GAS_LIMIT` (per the upgrade schedule, D-076) and cannot exceed it (`-32602`). A node runs at most 4 calls at a time; further calls return `-32009` until one finishes (local policy, D-098).
  `funds` (D-073): attached funds `[{ "asset", "amount" }]` (`amount` is a decimal string), simulated as a transfer from `caller` to the contract, empty by default;
  the receipt is a failure when the balance of `caller` is insufficient.
- Receipt: `{ "op_id", "success", "error", "gas_used", "output", "events": [{ "contract", "data" }], "asset_changes" }`; on failure `gas_used` is `max_gas`.
  - `asset_changes` (D-122): the changes the op made to built-in assets, in the order it made them, each with its `kind`, the `asset` and `events`, the number of the op's events
    recorded before it (a JSON number), so that the two lists interleave as the op made them. The kinds, with their other fields (accounts in hex, amounts as decimal strings):
    - `transfer` (`from`, `to`, `amount`): the `AssetTransfer` action, from its signer;
    - `funds` (`from`, `to`, `amount`): funds attached by the signer to a `Call` or a `Deploy`, by the `caller` to `ksana_call`, or by a contract to the contract it calls
      (`call_contract`, `try_call_contract`) or creates (`create_contract`);
    - `payout` (`from`, `to`, `amount`): a contract's `asset_transfer`;
    - `issue` (`to`, `amount`): an issuance by the asset's issuing contract (`asset_issue`, `asset_create`);
    - `burn` (`from`, `amount`): `asset_burn`;
    - `create` (`capabilities`): the asset's creation, by `asset_create` or by its first `asset_issue` (with `mint` alone), its capabilities named as `ksana_getAsset` names
      them; its issuance follows as an `issue`, unless it is of 0;
    - `freeze` (`account`, `frozen`), `allow` (`account`, `allowed`) and `pause` (`paused`): the issuing contract's management functions (D-121), as called;
    - `renounce` (`capabilities`): the capabilities the issuing contract gave up, those of the call the asset still had; none when it had none of them, as a management
      call is recorded as made.

    Empty when the op failed; a level that `try_call_contract` reverts takes its changes with it. KSA's fees, unlocks and rewards are not changes, nor is a movement of 0.
    `null` in the receipts the node stored before it recorded them (D-122); a node from before has no such field.

- `ksana_getSettlement`: the settlement transactions carried by the block (`txs`: `{ "commit", "reveal" }`, the commit and reveal transactions, Bitcoin network serialization, D-081). `txid` is
  the ID of the reveal transaction (the one whose witness carries the payload), `commit_txid` the ID of the commit transaction, both in Bitcoin internal byte order (D-001, consistent with the other hashes of this interface);
  `txid_rpc` / `commit_txid_rpc` are in the display byte order used by the bitcoind RPC and block explorers. `status`: `pending` (fewer than
  `SETTLEMENT_DEPTH` fast-lane confirmations, not yet broadcast, D-037), `broadcast` (broadcast, not yet confirmed), `confirmed` (the block's data has landed on the BTC main chain: the earliest transaction that landed its
  body, which may be the settlement transaction it carries itself, a rescue settlement or the batched settlement of a later block, D-077; for a block that needs no body, such as an empty block
  carrying the settlement transactions of a batch, the earliest transaction carrying its header at the BTC height where the header counts, which may be a later block's settlement or a republish,
  so that `confirmed.txid` can differ from `txid`; a transaction that confirmed without landing the data, because a header it needs had not counted yet, leaves the status at `broadcast` until a
  rescue lands it, D-135); `confirmed` gives that transaction's (the reveal transaction's) `{ "txid", "height", "block", "confirmations" }`. `settled`: the main-chain block that accepted the block has entered the settled closure (D-038, D-063); `btc_final`: the main-chain block that accepted the block is BTC-final (D-082, D-114). The block identifier can be the hash of any block in the DAG; for a block not yet accepted by the main chain,
  `settled` and `btc_final` are false.
  - Settlement batching (D-077): a block's body can be carried by the settlement transactions of a later block, so the block need not carry settlement transactions itself. For such a block, `carrier` is
    the block that carries its body (`{ "height", "hash" }`), and `txid` / `txid_rpc` / `commit_txid` / `commit_txid_rpc` / `txs` / `status` are those of the settlement transactions
    carried by that block; while no block carries it yet, these fields and `carrier` are all `null` and `status` is `pending`. For a block that carries settlement transactions, `carrier` is `null`.
  - A block that needs no body (an empty block, or a block containing only forced-inclusion entries, D-043) returns error `-32004` when it carries no settlement transactions.
  - `carrier_account` (D-095): the block's carrier -- the payee of the sender of the payload that first landed its data on BTC (for a settlement payload, the producer of the block it belongs to;
    for a republish payload, its `payee`, and the block's data must have waited more than `ANCHOR_DEPTH + CARRIER_REPOST_DELAY` BTC blocks after the anchor), who at unlock time takes from each of the block's
    reward slots a `CARRIER_BPS` share and from the path B sponsored fees a `CARRIER_FEE_BPS` share; `null` when there is no carrier (the reward goes to the producer). Answered according to the current BTC best chain and
    the current main chain; consensus decides by the anchor chain and the main chain at unlock time, and the two may differ after a BTC reorg or a main-chain switch.

- `ksana_estimateFee` (§8.2, D-054, D-078; a suggestion only, it does not change the conditions under which the node accepts ops; no parameters):
  - `feerate_sat_vb`: the fee rate at which this node funds settlements now (sat/vB, D-102, D-117): bitcoind's `estimatesmartfee` for `--settlement-conf-target` kept within `--settlement-feerate` (the floor, not below
    `MIN_SETTLEMENT_FEERATE`) and `--max-settlement-feerate` (the cap), the floor when bitcoind has no estimate; the floor, or bitcoind's `mempoolminfee` when that is higher, while bitcoind's mempool holds at most
    half a block (500 000 vB); path C native transactions and top-up transactions can use it as a reference.
  - `path_b.per_kib` (the local-policy patch of D-095): the suggested fee rate -- the token amount per KiB of cost. Cost = max(encoded VmOp bytes, `max_gas × MAX_BLOCK_DATA_BYTES /
    BLOCK_GAS_LIMIT`), the same as used for ordering this node's packing candidates (local policy: ordered by fee rate rather than absolute amount, so an op whose `max_gas` fills a whole block cannot squeeze out other ops with a slightly higher
    amount). It is the median, over the last 20 main-chain blocks, of each block's lowest path B entry fee rate (the larger middle value for an even count; blocks without path B entries
    are not counted), not below this node's `--min-fee-per-kib`.
  - `path_b.amount`: the suggested amount for an ordinary transfer (a KSA transfer, BIP340 signature, `max_gas` 10 000) converted from `per_kib` (rounded up), not below
    this node's `--min-token-fee`. Other ops are estimated as their own cost multiplied by `per_kib`.
  - `at`: the tip the estimate is based on.

## Indexer JSON-RPC

The standalone indexer (`ksana-indexer --rpc-addr`, default `127.0.0.1:8546`; D-048) connects only to bitcoind and provides two methods, with the same conventions as above:

| Method | Parameters | Result |
| --- | --- | --- |
| `indexer_status` | none | `{ "height", "hash", "state_root", "btc_height", "finalized", "halted" }` |
| `indexer_stateRoot` | block identifier (decimal height, `"latest"` or block hash) | `{ "height", "hash", "state_root" }` |

- `height` / `hash` / `state_root`: the highest block that has been executed and whose `state_root` has been verified (inside the settled closure and with at least `SETTLEMENT_DEPTH` confirmations on the indexer's main chain
  or on the finalized chain, D-038, D-063, D-082; so it is usually `SETTLEMENT_DEPTH` blocks below the node's settled prefix). `btc_height`: the height of the scanned BTC tip
  (`null` until it has synced with bitcoind). `finalized`: the BTC-final tip (`{ "height", "hash" }`, D-082, D-114), determined from the BTC chain data by the same rule as the node's `ksana_getChainInfo`
  field of the same name. `halted`: the reason processing stopped, a block of the finalized chain found invalid or unable to land (forced inclusion mismatch, settlement backlog
  limit, invalid execution, `state_root` mismatch; D-082, D-101); `null` in normal operation (the alert of §10.3 item 5). An invalid block not yet final does not halt it: it is passed over and the main
  chain chosen again without it (D-098), so the indexer stays below it or follows another branch, and publishes no `state_root` its re-execution did not give.
- `indexer_stateRoot` answers only for blocks that have been executed and verified; other blocks return error `-32004`, and a malformed identifier returns `-32602`.

## Error codes

| Code | Meaning |
| --- | --- |
| `-32601` | No such method, or not available (for instance `ksana_getAccountOps` on a node from before D-111, `ksana_getToken` on one from before D-120, or either on a node whose operation index and standards index are not built) |
| `-32602` | Malformed parameter (hex, decoding, length) |
| `-32010` | Invalid op (violates a consensus rule) |
| `-32011` | Op does not meet this node's policy (fee too low, pool full, replacement fee not high enough) |
| `-32004` | The queried object does not exist |
| `-32009` | The node is busy (`ksana_call`: too many calls running at once); try again later |
| `-32603` | Internal node error |
| `-32006` | Too many subscriptions on this WebSocket connection (jsonrpsee, [Limits](#limits)) |
| `-32007` | The request body or WebSocket message is longer than 1 MiB (jsonrpsee, [Limits](#limits)) |
| `-32008` | The response would be longer than 10 MiB (jsonrpsee, [Limits](#limits)) |
| `-32010`, `-32011` with `id` null | The batch has more than 64 calls, or its answers would pass 10 MiB (jsonrpsee, [Limits](#limits)) |
