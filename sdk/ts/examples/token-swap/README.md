# Token and pool from the command line (token-swap)

A Node.js command-line example of the SDK: deploys the `asset-token` example contract as a token (a VM built-in asset,
D-073), a pool of the `amm` example between two built-in assets, adds liquidity, swaps, and finds the Bitcoin transaction that
settled each op. Every op is path B, signed with a secret key file (BIP340, the same file as `ksana-keytool` and `ksana-cli`) and
submitted with `ksana_submitOp`; each command waits until its op is executed and prints what it did. The network is the node's:
the tool reads the node's chain ID (`getParams`, D-106) and signs for it, and with `--chain` it refuses a node on another network.

Because the token is a built-in asset, wallets and the explorer show its balances with its name like KSA's, holders send it with
`AssetTransfer`, and the pool takes it as attached funds with no approve step (`contracts/examples/asset-token`).

## Build

```sh
(cd sdk/ts && npm ci && npm run build)        # → sdk/ts/dist/examples/token-swap/main.js
cd contracts
cargo rustc --release --locked --target wasm32-unknown-unknown --package asset-token --crate-type cdylib
cargo rustc --release --locked --target wasm32-unknown-unknown --package amm --crate-type cdylib
# → contracts/target/wasm32-unknown-unknown/release/asset_token.wasm and amm.wasm
```

## Commands

```text
token-swap key <file>                       create (when missing) or read a secret key; print its account and address
token-swap token   --key <file> --code asset_token.wasm --name <name> --symbol <symbol> --decimals <n> --supply <amount>
token-swap pool    --key <file> --code amm.wasm --a <asset> --b <asset>
token-swap add     --key <file> --pool <contract> --a <amount> --b <amount> [--slippage <percent> | --min-shares <shares>]
token-swap swap    --key <file> --pool <contract> --in <asset> --amount <amount> [--slippage <percent>]
token-swap balance <account>                balances with the assets' names
token-swap settlement <op_id>...            the block of each op and the BTC transaction that settled it
```

`token-swap` stands for `node sdk/ts/dist/examples/token-swap/main.js`. `<asset>` is `KSA` or an asset ID (64 hex digits),
`<account>` a Taproot address or 64 hex digits; amounts are in whole units of the asset, decimals allowed. Each op gets
`--max-gas` (default 1 000 000, enough to deploy either contract). The token's name is 1 to 64 bytes, its symbol 1 to 16 and its
decimals at most 18, the limits the protocol keeps for every asset (D-121); its supply is fixed for good, since the contract gives
it none of the issuer capabilities.

### What the node decides

The node is `--rpc <url>`, else `$KSANA_RPC`, else `http://127.0.0.1:8545`. Its answers decide much of what the tool signs:

- **The network.** `--chain devnet|signet|mainnet`, else `$KSANA_CHAIN`, is the network the tool signs for, and a node on
  another network is refused before anything is signed. Without either, the tool warns and signs for the network the node
  reports. A key is the same account on every network.
- **The fee.** Each op pays the fee the node's `ksana_estimateFee` asks for an op of its size, at most `--max-fee` (default
  0.01 KSA): a node asking more is refused before anything is signed. The ask follows the fee rates recent blocks took, so
  paying a margin above it would only raise the next ask.
- **The quotes.** The minimums of `swap` and `add` come from read-only calls on the node, so use a node you trust. Plain
  `http://` to another machine gets a warning, since anyone on the way could change the answers. What `add` attaches comes
  from its quote too, but never more than the amounts given: a quote that takes more than an amount, or none of it, is
  refused before anything is signed.

### Minimums

- `swap` quotes the trade with a read-only `ksana_call` as the signing account, and sets the op's minimum output `--slippage`
  percent below the quote (default 1).
- `add` quotes the deposit the same way. The op offers only the amounts the pool's ratio takes of those given (the pool would
  refund the rest), with `min_shares` `--slippage` percent below the quoted shares, or `--min-shares`. Amounts in the pool's
  ratio get fewer shares whichever way a swap just before the deposit moves the price, so the minimum stops a deposit such a swap
  would make lose. `--min-shares 0` sends no minimum: the two arguments that a pool of the `amm` example built before
  `min_shares` takes.

### Waiting

Each command waits up to 180 s for its op. A block the main chain merges may skip an op, which then returns to the entry pool
while it stays valid (D-063), so the command keeps waiting for it. When the wait ends first, the command says where the op
stands and exits with status 2: still in the entry pool or skipped, it has not failed and may still execute; no longer known to
the node, it left the entry pool unexecuted, as when another op takes its nonce. Check it with `token-swap settlement <op_id>`
before running the command again, since once the op executes a second run does the same again. To cancel it, replace its nonce
with any op paying a higher fee (`ksana-cli` takes `--nonce`). An error exits with status 1.

### Text from the chain

Asset names, symbols and revert reasons come from the chain, where anyone can write them. The tool escapes control and other
non-printing characters before printing them, and prints every asset but KSA with the start of its ID: any contract may name its
asset "KSA", but only the asset with KSA's ID prints as plain `KSA`. Transaction IDs are printed from the bytes the SDK checks,
not from the display strings the node sends with them.

## The Wizards demo on the public signet

On 2026-10-14 these commands ran against the public signet's node, with two accounts funded by the signet's faucet:
`wizard.key` deploys the token and the pool and adds the liquidity, `trader.key` swaps. To run them there, set
`KSANA_RPC=https://signet.ksana.network/rpc` and `KSANA_CHAIN=signet` (the demo used the signet's former name,
D-112). The demo ran before `add` sent a minimum of shares: its pool runs the `amm` of that day, which
takes none, so adding to that pool takes `--min-shares 0`. It also ran before assets had a symbol (D-121): `token` now takes
`--symbol` as well.

```sh
token-swap token --key wizard.key --code asset_token.wasm --name Wizards --symbol WIZ --decimals 8 --supply 21000000
token-swap pool  --key wizard.key --code amm.wasm --a <the asset ID printed by token> --b KSA
token-swap add   --key wizard.key --pool <the pool printed by pool> --a 1000000 --b 50
token-swap swap  --key trader.key --pool <the pool> --in KSA --amount 5
token-swap settlement <op_id>...
```

| Step | Op | Block | Executed after | Result |
| --- | --- | --- | --- | --- |
| Deploy the token | `6130432c…771860ce` | #76 | 6.5 s | contract `517f47d9…7d37eb68`, asset `0841acce…c8f0494e`: 21,000,000 Wizards issued to the deployer |
| Deploy the pool | `c2b38dc3…23313104` | #77 | 2.1 s | pool `8d85e4e3…927b41c4` (Wizards / KSA) |
| Add liquidity | `549a3538…bc2320ae` | #78 | 2.4 s | 707,106,781,186 shares; reserves 1,000,000 Wizards / 50 KSA |
| Swap 5 KSA | `93f5ccf0…c454a9da` | #80 | 5.2 s | received 90,661.08938801 Wizards (the quote, at least 89,754.47849412) |

The trader then sent 10,000 Wizards from the hosted web wallet (`AssetTransfer` `36ae6f37…9aef8d0d`, block #81,
fast-confirmed 4.5 s after signing): the wallet lists the token by the name `ksana_getAsset` gives, with no configuration.

The producers then wrote these blocks into Bitcoin signet, two settlements in all; `token-swap settlement <op_id>...` finds
them. Each is the reveal transaction of a commit / reveal pair, carrying the blocks in its witness:

| Blocks | Bodies | Reveal transaction | BTC block |
| --- | --- | --- | --- |
| #75 – #77 | #76 and #77: the token and the pool | `f4ef4e80…fc18c76f` | #326,256 |
| #78 – #86 | #78, #80, #81 and #86: the liquidity, the swap and the send | `c61630eb…c95f16a6` | #326,257 |

The full IDs:

```text
token op     6130432cd6fd5b97ba862c8948539557538af267dfcb0d881b8235e2771860ce
contract     517f47d96fc555d18d5acfc43af7f047e7d802c2edafd99453a2887b7d37eb68
asset        0841acceccfb84021c2cabfc3e05042cd58a2398438e44f8f0b6ea9cc8f0494e
pool op      c2b38dc323c9b837ae3a044cf6429c361e076108d4eb80dd398610e923313104
pool         8d85e4e326eae76d3b8cfbedd5b3ed13879f360c18ab1248e439176d927b41c4
add op       549a3538d7a56a7a5490f819b11cee11d10f402ace3f9b2884d2f06bbc2320ae
swap op      93f5ccf078982083e66e66ea2b63b6dbe9183e576fb81cab8b17cd2dc454a9da
transfer op  36ae6f3717be516becb0e73a31ae97b4da81b84d0c6d60a6e04c90f09aef8d0d
reveal       f4ef4e80f8efb569b5fad0108f1655dc17edc17d03bc16681a66a1e6fc18c76f  blocks #75 – #77
commit       1abbd373e58f1c216a60a8e5b6998afcdaeac00da0a79200b756afa42912091f
reveal       c61630eb466ac2a71fe026925826111f40bf632efc4978d14be97bf4c95f16a6  blocks #78 – #86
commit       b613788b9ebf029c6331ad554e93c2560768fc4bcf33f851ec1f42d121c97fe0
```
