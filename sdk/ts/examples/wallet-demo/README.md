# Wallet demo page (M6, D-055; path B since D-078)

A static page: connects Unisat or Xverse, queries the account and the suggested fee, has the wallet sign the VmOp's sighash with
BIP322, submits a KSA transfer paying the fee in KSA (path B, `FeeMode::Token`), and tracks the status of the operation and the
settlement transaction. The page only calls the SDK (`signVmopWithWallet` from `src/wallet/flow.ts` and `KsanaRpcClient` from
`src/rpc.ts`) and is bundled into a single file by esbuild.

Path A (the wallet signs a funding input that is merged into the producer's settlement transaction) was dropped per D-078, and the
M6 acceptance 2 steps in `docs/SIGNET.md` that used path A are void with it. To pay with BTC, first top up for KSA (D-085): the
seller locks KSA for the buyer in the exchange contract (`ksana-cli exchange lock`), then the buyer pays and claims the KSA with
one path C transaction (the SDK's `buildTopUp`, see the SDK README; `ksana-cli top-up --lock`). This page has no top-up button;
the wallet account's KSA can be topped up as above, or on devnet transferred in from a genesis-allocated account with
`ksana-cli transfer --to <the wallet's Ksana account>`.

## Build and open

```sh
cd sdk/ts
npm ci
npm run build:demo                                    # → examples/wallet-demo/dist/wallet-demo.js
npx --yes http-server examples/wallet-demo -p 8080 -a 127.0.0.1    # or python3 -m http.server -d examples/wallet-demo 8080
```

Open `http://127.0.0.1:8080/` in the browser (wallet extensions do not inject their API into `file://` pages, so it must be opened
over HTTP). The page and the node RPC have different origins, so the node must be started with `--rpc-cors '*'` (or
`--rpc-cors http://127.0.0.1:8080`).

## Usage

1. Fill in the node RPC address and choose the network (signet: chain ID 1002; devnet / regtest: 1001). Before signing, the page
   reads the node's chain ID and refuses a node on another network (`getParams`, D-106).
2. "Connect Unisat" or "Connect Xverse": the wallet switches to the corresponding Bitcoin network; the address type must be Taproot
   (P2TR; Xverse uses the ordinals address, which is P2TR): the account is this address (D-092), and the page shows the address
   and its hex form (the account), the nonce and the KSA balance.
3. Fill in the recipient account (the recipient's Taproot address or 64 hex digits) and the amount (KSA base units); both the
   amount and the fee are paid from the wallet account's KSA balance, and the page does not submit when the balance is
   insufficient.
4. "Estimate fee" calls `ksana_estimateFee` and fills in the suggested path B fee (the median path B fee of the recent main-chain
   blocks, not below the node's minimum; when left empty, it is estimated automatically at submission).
5. "Sign and submit": the wallet pops up a BIP322 message signature (the message is the hex of the VmOp sighash); no PSBT
   signature is needed and none of the wallet's UTXOs are spent. The page verifies the signature locally, then calls
   `ksana_submitOp` and afterwards refreshes the operation status (pending → fast_confirmed → btc_final) and the settlement
   transaction every 2 seconds (the txid can be looked up in a block explorer; when the block carries no settlement transaction,
   the block that carries its body is shown, D-077).

## Mock wallet for local testing

When `?mock=p2tr&key=<hex of a 32-byte private key>` is appended to the page URL (optionally `network=regtest|signet`, `rpc=…`,
`chain=devnet|signet`), the page injects the SDK's mock wallet (`src/wallet/mock.ts`, which implements the same interface as
Unisat) and needs no browser extension.
**For local regtest only**: the private key appears in the address bar. `test/e2e/demo-page.ts` uses this to run through the page
automatically in Chromium (`tests/integration/tests/m6_regtest.rs`).
