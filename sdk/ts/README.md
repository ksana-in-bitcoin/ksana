# @ksana/sdk: the Ksana TypeScript SDK

SPEC §11.3, DECISIONS D-053. Builds, encodes and signs VmOps (BIP340, or BIP322 simple signatures by browser wallets), builds
path C native transactions, calls the JSON-RPC of the node and the indexer, and adapts the Unisat / Xverse wallets. The Borsh
encoding matches the Rust side byte for byte, verified by the shared vectors in `tests/vectors/`.

Two submission paths: path B (sponsored, `FeeMode::Token`) pays the fee in KSA, is submitted directly with `ksana_submitOp` once
signed, and takes effect within seconds; path C (native transaction, `FeeMode::BtcNative`) is carried by a BTC transaction and is
force-included once it confirms. Path A (merging a user-signed input/output pair into the settlement transaction) was dropped per
D-078: to pay with BTC, first top up for KSA (the seller locks KSA for the buyer in the exchange contract, the buyer pays and
claims with one path C transaction, see "Top-up" below), then use path B.

The package is `private` (not published), ESM + TypeScript. The library code uses only Web standard APIs (`fetch`,
`crypto.getRandomValues`, `TextEncoder`) and works in both Node.js (≥ 20.19) and browsers; `npm run lint` checks `src/` separately
without loading the Node types.

## Build and test

```sh
cd sdk/ts
npm ci              # dependency versions are locked in package-lock.json
npm run lint        # tsc --noEmit: src/ uses only Web APIs (tsconfig.lib.json), then src/ and test/ are strictly checked together
npm run build       # tsc → dist/src (library) and dist/test (tests)
npm test            # node --test, runs on the compiled output
```

The CI `ts sdk` job (Node 22) runs these four steps in order. The tests read the repository root's `tests/vectors/` and
`test/data/bip322/` relative to the location of the compiled files, independent of the current directory.

Using it in a browser: the library code is standard ESM and imports its dependencies by package name (`@noble/curves/secp256k1.js`
etc.), so use a bundler (esbuild, Vite, etc.) or an import map.

## Conventions

- Types correspond field by field to Rust (`crates/ksana-primitives/src/types.rs`), with the same field names as serde (`chain_id`,
  `max_gas`, `btc_txid`, ...). Enums are unions tagged with `kind` (`{ kind: 'Token', amount: 5n }`, `{ kind: 'BtcNative' }`); the
  contents of tuple variants are in the `bytes` field (`{ kind: 'Bip340', bytes }`).
- `u64` / `u128` are `bigint`, `u8` / `u32` are `number`, byte strings are `Uint8Array` (Node.js `Buffer` is accepted too).
- BTC transaction IDs and block hashes are always in **Bitcoin internal byte order** (D-001); bitcoind RPC, wallets and block
  explorers display the reversed order, convert with `txidFromRpc` / `txidToRpc`.
- JSON (D-007): `toJson` / `fromJson` match `tests/vectors/`: byte strings are lowercase hex, `u64` / `u128` are decimal strings,
  enums are externally tagged; unknown fields, missing fields and non-canonical decimal notation are rejected.
- Decoding external input checks the length limit first (`decodeVmOp` uses `MAX_OP_BYTES`, `decodePayload` uses
  `MAX_BLOCK_DATA_BYTES`) and requires the input to be fully consumed and canonically encoded.
- Nothing defaults to a network (A2-11, D-106): what signs, builds or decodes takes the parameters of the node it talks to.
  `await rpc.getParams()` picks them by the node's chain ID (`ksana_getChainInfo`); `await rpc.getParams(SIGNET)` also refuses a
  node on another network with `ChainMismatchError`, and so does a chain ID of no known network. The KSN1 magic is the same on
  every network, so a path C operation signed for another network would still be force-included and fail on its chain ID while
  its BTC payment confirms: `buildNativeTx` / `nativePayload` refuse an operation whose chain ID is not that of `params`.

## Modules

| Module | Contents |
| --- | --- |
| `params` | `DEVNET` (chain ID 1001), `SIGNET` (1002, D-055), `MAINNET` (1000, D-096), `NETWORKS`, `networkOfChainId` / `paramsForChainId` (the network of a chain ID; a mismatch throws `ChainMismatchError`, D-106), `VMOP_VERSION`, `ksaAssetId()`; `pow_algorithm` (the Argon2id parameters, D-094) |
| `types`, `codec` | Consensus types; `encode` / `decode(type, bytes, maxLength)` (every type but the payload), `encodeVmOp` / `decodeVmOp(bytes, params)`, `encodePayload(payload, params)` / `decodePayload(bytes, params)` (compressed block headers and entries in payloads, D-075, D-079, D-080; VmOps in entries omit the chain ID and are encoded / decoded per `params`), `toJson` / `fromJson` |
| `hash` | `taggedHash`, `vmopSigningMessage` / `vmopSighash` (`Ksana/VmOp/v1`; path C operations carry the carrying transaction's first input `firstInput`, D-086), `encodeOutPoint`, `opId`, `blockHash`, `merkleRoot` (RFC 6962), `entriesRoot` |
| `keys`, `sign` | `accountOf` (account = the Taproot output key of the private key after the BIP86 tweak, i.e. the witness program of the `bc1p` address, D-092), `xOnlyPublicKey` (internal key), BIP86 tweak; `signVmop` / `signedVmop` (BIP340, signs with the tweaked private key), `verifyVmop` (returns `'unsupported'` for `Bip322Simple`; the Rust side is authoritative) |
| `bip322` | `bip322Message` (lowercase hex of the sighash, D-052; path C carries `firstInput`), `bip322ToSpend` / `bip322ToSign`, local signing `signBip322Simple` / `signVmopBip322` (the account's Taproot address, D-092), `decodeBip322Signature`, local self-checks `verifyBip322ForAccount` / `verifyVmopBip322` |
| `pathC` | `ksn1DataScript`, `parseKsn1DataScript`, `nativePayload`, `nativeFirstInput` / `nativeTxFirstInput` / `isBoundTo` (the first input the signature is bound to, D-086), `buildNativeTx` (unsigned PSBT, optionally with payment outputs; the operation's signature must be bound to `utxos[0]`), `signNativeTx`, `nativeOpFromTx`; `txData` / `txPayload` (a transaction's data carrier as the node reads it: the KSN1 outputs and the envelopes in the inputs counted together, exactly one, a KSN1 output carrying only a path C payload and an envelope only a settlement or republish one, D-081, D-133) |
| `envelope` | The KSN1 envelope in a Taproot witness that carries settlement and republish payloads (D-081): `ksn1EnvelopeScript` (the leaf script), `witnessLeaf` (a witness's leaf script, the annex left out), `isKsn1Envelope`, `parseKsn1Envelope` |
| `pow` | PoW verification (D-094): `decodeCompact` / `targetFromBits` / `effectiveTarget` (the compact encoding of `bits`, the PoW limit and the weighted effective target, D-084), `powSalt` (`"ksana-pow" ‖ chain ID`), `powHash` / `powValue` (the 32-byte Argon2id output and its value interpreted as little-endian, using `argon2id` from `@noble/hashes`, byte-for-byte identical to Rust), `checkPow(params, header, weight)`; verification only, no solving |
| `topUp` | Top-up (D-078, D-085): `getLock` / `decodeLock` / `encodeLock` (the exchange contract's lock), `lockArgs` (the seller's lock arguments), `claimArgs`, `quoteTopUp`, `buildTopUp` (the claim operation signed by a one-time key + the PSBT of the payment transaction; rejects locks whose beneficiary is not the buyer, that are not `Open`, that are near expiry, or that would be over- or underpaid; returns `payBy`, the BTC height before which the payment must confirm) |
| `standards` | The token standards KS-20 and KS-721 (`docs/STANDARDS.md`, D-120): the operations as `Action`s (`ks20Transfer`, `ks20TransferFrom`, `ks20Approve` with an optional expiry, `ks20TransferAndCall`, `ks20Burn` / `ks20BurnFrom`, `ks20Mint` / `ks20SetMinter`; `ks721Transfer`, `ks721TransferFrom`, `ks721Approve`, `ks721SetOperator`, `ks721TransferAndCall`, `ks721Burn`; `assetTransfer` for an `AssetRef` of either kind), which refuse a zero amount and the zero account or the contract itself as recipient before an op is signed; the views through the node's read-only call (`readStandards`, `readKs20` / `readKs721`, `readCapabilities` and one function per view, read one at a time; a refusal throws `StandardCallError` with the contract's reason); the events (`decodeKs20Event` / `decodeKs721Event` / `decodeStandardEvent`, `null` for bytes that are no event of the standard, and the encoders); KS-721 token IDs from serial numbers (`ks721TokenId` / `ks721Serial`); `encodeAssetRef` / `decodeAssetRef`; `STANDARD_REASONS`, `STANDARD_CAPABILITIES` |
| `btc/*` | Transaction serialization, BIP143 / BIP341 sighashes, addresses, account ↔ Taproot address (`accountToAddress` / `accountFromAddress` / `parseAccount`: an address or 64 hex digits with an optional lower-case `0x`, the whitespace around it removed as Rust's `str::trim` does, `btcNetworkOf`, D-092, D-133), scripts and the dust limit, signing PSBTs with a local private key (`signPsbtWithKey`, `extractTransaction`) |
| `rpc` | `KsanaRpcClient` (`ksana_submitOp`, `getOpStatus`, `getBlock`, `getAccount`, `getAsset` (with the symbol and the issuer capabilities, D-121), `getAssetStatus` (an account's standing in an asset, D-121), `getChainInfo`, `getParams` (the parameters of the node's network, D-106), `call`, `getSettlement`, `estimateFee`, the operation listings `getAccountOps` / `getLatestOps` (D-111), and the standards index `getContract`, `getAccountAssets`, `getToken`, `getTokenHolders`, `getNfts`, `getApprovals` (D-120)), `IndexerRpcClient`, `subscribeBlocks` (WebSocket); results are parsed strictly, rejecting unknown fields like the node does; errors are `RpcError` (with an error code) or `RpcTransportError` |
| `wallet/*` | The `WalletAdapter` interface, `UnisatAdapter`, `XverseAdapter` (the wallet must use a Taproot address, which is the account, D-092; `getNetwork` says which BTC network the wallet is set to, D-106), `signVmopWithWallet` (the wallet signs with BIP322 and the signature is self-checked locally), `MockUnisatWallet` (mock wallet) |

## Example: path B with Unisat

The wallet signs the VmOp's sighash with BIP322 (D-052); `signVmopWithWallet` self-checks the signature with the SDK's
implementation before returning. Both the fee and the transfer amount are paid from the account's KSA balance; the producer
sponsors the BTC fee.

```ts
import { KsanaRpcClient, SIGNET, UnisatAdapter, VMOP_VERSION, btcNetworkOf, fromHex, ksaAssetId, parseAccount, signVmopWithWallet, toHex } from '@ksana/sdk';

const rpc = new KsanaRpcClient('http://127.0.0.1:8545');
const params = await rpc.getParams(SIGNET); // the node's network: throws ChainMismatchError when the node is not on signet
const wallet = new UnisatAdapter(); // window.unisat
const { account } = await wallet.getAccount(); // { address, publicKey, addressType: 'p2tr', internalKey, account }
const accountKey = fromHex(account, 32); // account: the 32-byte output key inside address (bc1p…) itself (D-092)

const { nonce } = await rpc.getAccount(accountKey);
const fee = await rpc.estimateFee(); // path_b.amount: the suggested KSA fee
const op = await signVmopWithWallet(
  {
    version: VMOP_VERSION,
    chain_id: params.chain_id, // chain ID 1002
    account: accountKey,
    nonce,
    fee: { kind: 'Token', amount: fee.path_b.amount },
    max_gas: 100_000n,
    action: { kind: 'AssetTransfer', asset: ksaAssetId(), to: parseAccount('recipient Taproot address or 64 hex digits', btcNetworkOf(params)), amount: 1_000n },
  },
  wallet, // the wallet pops up a BIP322 message signature (the message is the hex of the VmOp sighash)
);
const { op_id } = await rpc.submitOp(op);
console.log(toHex(op_id), await rpc.getOpStatus(op_id)); // pending → fast_confirmed → btc_final
```

For Xverse use `new XverseAdapter()` (by default it uses ordinals, i.e. the P2TR address, which is the account; the payment address
is usually P2SH-P2WPKH and cannot be an account, D-092). Unisat's account address type must be switched to Taproot.

The address does not always pin the BTC network (`tb1` is testnet3's, testnet4's and signet's alike), so before relying on the
wallet's network ask it: `await wallet.getNetwork?.()` gives `{ network, name }` from Unisat's `getChain` or Xverse's
`wallet_getNetwork` (`network` is absent for a chain that is no SDK network, such as testnet4), or `undefined` when the wallet
cannot tell (D-106).

## Example: path B with a local private key

```ts
import { DEVNET, KsanaRpcClient, VMOP_VERSION, accountOf, fromHex, ksaAssetId, signedVmop, toHex } from '@ksana/sdk';

const secretKey = fromHex('…private key as 64 hex digits…', 32);
const rpc = new KsanaRpcClient('http://127.0.0.1:18545');
const params = await rpc.getParams(DEVNET); // refuses a node that is not on devnet
const account = accountOf(secretKey); // account: the BIP86-tweaked Taproot output key; accountToAddress(account, 'regtest') is its bcrt1p address

const { nonce } = await rpc.getAccount(account);
const fee = await rpc.estimateFee();
const op = signedVmop(
  {
    version: VMOP_VERSION,
    chain_id: params.chain_id,
    account,
    nonce,
    fee: { kind: 'Token', amount: fee.path_b.amount }, // the project token; the producer sponsors the BTC fee
    max_gas: 50_000n,
    action: { kind: 'AssetTransfer', asset: ksaAssetId(), to: fromHex('…32-byte account…', 32), amount: 5n },
  },
  secretKey, // BIP340 signature; aux_rand defaults to fresh randomness
);
const { op_id } = await rpc.submitOp(op);
console.log(toHex(op_id), await rpc.getOpStatus(op_id));
```

## Path C

The signature of a path C operation is bound to the first input of the BTC transaction that carries it (D-086): first pick
`utxos[0]`, sign with `signVmop` / `signVmopBip322` / `signVmopWithWallet(op, wallet, { firstInput })` using
`nativeFirstInput(utxos[0])` as `firstInput`, then call `buildNativeTx({ op, utxos, changeScript, feeRate, params })` with the
node's `params` (`rpc.getParams()`; it refuses to build when the signature is not bound to `utxos[0]` or `op.chain_id` is not that
of `params`). It returns an unsigned PSBT (output 0 is the KSN1 data output, output 1 the
change); hand it to the wallet to sign with `SIGHASH_ALL` (the equivalent `SIGHASH_DEFAULT` for P2TR) and broadcast; with a local
private key use `signNativeTx` + `extractTransaction`. `op.fee` must be `BtcNative`; the transaction is force-included after it
gets `NATIVE_CONFIRMATIONS` confirmations; an operation copied into another transaction fails authentication and does not consume
the nonce. The KSN1 data output can be far larger than 83 bytes: nodes before Bitcoin Core v30 only relay it with
`-datacarriersize` (the devnet regtest is already configured, SPEC §7.1).

## Top-up (D-078, D-085)

The flow of the exchange contract (example `contracts/examples/ksa-exchange`) is "order → lock → confirm → redeem":

1. **Order** (off-chain): buyer and seller agree on the KSA amount, the price (how many KSA base units per sat) and the buyer's
   Ksana account.
2. **Lock** (seller, path B): the seller calls the contract's `lock(ksa, price, script, beneficiary, expiry)` (build the arguments
   with `lockArgs`; KSA can be attached to the call), locking the KSA for the buyer's account and stating the price, the BTC
   payment script used only for this lock (a script can be used only once; use a fresh address for every lock) and the expiry BTC
   height; the receipt's output is the lock ID (u64). The seller cannot take back the locked KSA and the terms cannot be changed;
   only when `RELEASE_DELAY` (6) more BTC blocks after expiry have passed with nobody paying can the seller `release`.
3. **Confirm** (buyer, path C): `getLock(rpc, exchange, lockId)` reads the lock, `buildTopUp({ exchange, lockId, lock, beneficiary,
   sats, currentBtcHeight, utxos, changeScript, feeRate, params })` (the node's `params`, from `rpc.getParams()`) builds a native
   transaction: output 0 is the KSN1 data output carrying
   the claim operation (`claim(lockId)`), output 1 pays the payment script, followed by the change. The claim operation is signed
   by a randomly generated one-time key with nonce 0: when a path C nonce is wrong the operation is void while the BTC has already
   been paid; a one-time account does not take up the wallet account's nonce, and the confirmation order of several top-ups does
   not affect each other; the wallet only needs to sign this BTC transaction. The claim operation carries only the lock ID; where
   the KSA goes is fixed in the lock. `buildTopUp` refuses to build when the lock's beneficiary is not `beneficiary` (the buyer's
   own account, otherwise the KSA would be credited to someone else), the lock is not `Open`, the payment differs from the
   `satsForAll` given by `quoteTopUp` (overpaying buys no more, `allowOverpay: true` allows it; underpaying buys only a part and
   leaves the rest locked until another payment buys it or the seller releases it, `allowPartial: true` allows it), or `currentBtcHeight + expiryMargin ≥ lock.expiry` (default margin
   `DEFAULT_EXPIRY_MARGIN` = 3 blocks: the payment must be force-included before the seller can release). The returned `payBy` is
   the height of the BTC block by which (inclusive) the payment transaction must be confirmed (`expiry + RELEASE_DELAY −
   native_confirmations − 1`): past it the payment may still make it in time, but there is no longer a guarantee.
   The claim operation's signature is bound to the first input of the payment transaction (`utxos[0]`, D-086): if someone else
   (including the seller) copies it into a transaction that does not pay, the node judges it as failing authentication and the
   one-time key's nonce is not consumed.
4. **Redeem**: after BTC confirmation the claim operation is force-included, the contract credits `min(sats × price, lock.ksa)` to
   the beneficiary account fixed at lock time (`lock.ksa` is the KSA still locked), and KSA the payment did not buy stays locked
   for the beneficiary (D-098: anyone can pay a lock's script and claim, but the KSA always goes to the beneficiary, so a seller
   paying its own script a few sats first cannot take the rest back). Track the claim operation with `draft.opId` and check the
   lock and the beneficiary's balance; once credited, path B is available.

```ts
const params = await rpc.getParams(SIGNET); // the claim is signed with this network's chain ID; refuses a node on another network
const lock = await getLock(rpc, exchange, 0n);
const { header } = await rpc.getBlock('latest'); // BTC height for the expiry check; use the chain tip height when the wallet has the BTC chain tip
const draft = buildTopUp({
  exchange, lockId: 0n, lock, beneficiary: myAccount, sats: 20_000n, currentBtcHeight: header.btc_anchor.height,
  utxos, changeScript, feeRate: 2, params,
});
// draft.payBy: the payment must be confirmed by this BTC height (inclusive), otherwise the seller may release first
const signed = await wallet.signPsbt(draft.psbt, draft.inputs.map((_, index) => ({ index })));
// broadcast extractTransaction(signed); after BTC confirmation rpc.getOpStatus(draft.opId) is fast_confirmed and the KSA is credited to lock.beneficiary
```

## Token standards (D-120)

A contract says which standard it follows in its declaration, `standards()`; the views ask it and the rest through the node's
read-only call (`ksana_call`), and an operation is an `Action` signed into a VmOp like any other. Every token refuses a zero
amount and the zero account or the token itself as recipient, so the builders refuse them first rather than let the op fail
on chain for its fee.

```ts
import { KsanaRpcClient, decodeStandardEvent, fromHex, ks20BalanceOf, ks20Transfer, readKs20, toHex } from '@ksana/sdk';

const rpc = new KsanaRpcClient('http://127.0.0.1:8545');
const token = fromHex('…the token contract ID as 64 hex digits…', 32);
const info = await readKs20(rpc, token); // null unless the contract declares KS-20
if (info !== null) {
  // name, symbol, decimals, total_supply, and what its rules let someone do now: ['mint', 'fee'], say (docs/STANDARDS.md §5.2)
  console.log(info.symbol, info.decimals, info.capabilities);
  console.log(await ks20BalanceOf(rpc, token, account)); // in base units, as a bigint
  const action = ks20Transfer(token, recipient, 5n); // a Call of `transfer`: sign it as in the examples above
  // ...after the op executes, its receipt's events, by the declaration of the contract that emitted them
  for (const event of receipt.events) {
    if (toHex(event.contract) === toHex(token)) console.log(decodeStandardEvent(info.standards, event.data));
  }
}
```

`ks20TransferAndCall` moves tokens into a contract and calls its `on_token_received` in one op (the funds attached to the
call are passed on, and the receipt's output is the recipient's answer, `decodeTransferAndCallAnswer`). A contract that
refuses a view throws `StandardCallError` (`method`, `reason`); `readStandards` answers `null` instead, for a contract of
no standard. KS-721 token IDs are 32 bytes: `ks721TokenId(7n)` is serial number 7, and `ks721Serial` gives it back, or
`null` for an ID that holds something else (a hash, say).

## Test coverage

- `tests/vectors/codec.json`: for every valid vector, JSON → Borsh and Borsh → JSON are identical byte for byte / field by field,
  plus `op_id` and `block_hash`; every enum variant has a vector; every invalid vector fails to decode.
- `tests/vectors/signing.json`: tagged hash; sighashes and BIP340 signatures are reproduced byte for byte; verification fails after
  tampering; for the `bip322` cases the message, `message_hash`, the `to_spend` / `to_sign` transaction IDs and the witness (both
  local signing and the mock wallet reproduce them byte for byte).
- `tests/vectors/merkle.json`: RFC 6962 roots and `entries_root`.
- `tests/vectors/standards.json` (D-120): every method's arguments through the operation builders and the views, every answer
  through the views (and refused with a byte more), every event decoded and encoded again and through `decodeStandardEvent`,
  every invalid event decoded to `null`, KS-721 token IDs and `AssetRef`; the reasons and capabilities against the tables of
  `docs/STANDARDS.md`; the builders' refusals, `readKs20` / `readKs721` asking one view at a time, and a refused view.
- `tests/vectors/pow.json` (D-094): for every case the salt, the Argon2id output `pow_hash`, `pow_value`, the effective target and
  whether it is met are reproduced one by one; compact decoding and the weighted effective target.
- BIP322 official vectors (`test/data/bip322/`, from bitcoin/bips): message hashes and transaction IDs; all P2TR and P2WPKH simple
  signatures verify (the P2TR ones also verify per account; the P2WPKH RFC 6979 signature assembled per BIP322 is byte-for-byte
  identical to one of the official vectors, but per-account verification always fails, D-092); all other types and error cases
  fail.
- BIP143 / BIP341 sighashes are cross-checked against the independent implementation in `@scure/btc-signer`; chunking and push
  encoding of KSN1 scripts; building, signing and recognizing native transactions; the RPC client (request shape, strict result
  parsing); wallet adapters (the BTC network each says the wallet is set to included), the mock wallet's PSBT signing (finalized and
  not finalized, signing only this account's inputs) and BIP322 signing, path B with the mock wallet; top-up (lock encoding / decoding, `lock` / `claim` arguments, quotes, transaction
  structure and rejection conditions).
- The network (`network.test.ts`, D-106): networks by chain ID, `getParams` refusing a node on another network or on none, path C
  transactions refused for an operation signed for another network, the claim of a top-up signed for the network it is built for,
  and no generic Borsh codec for payloads.

## Wallet demo page and end-to-end tests

- `examples/wallet-demo`: a static page that connects Unisat or Xverse and uses path B (D-055, D-078); `npm run build:demo` bundles
  it with esbuild, see its README.md for usage.
- `test/e2e/`: regtest end-to-end scripts called by `tests/integration/tests/m6_regtest.rs` and `m13_regtest.rs`
  (`tests/integration/run.sh` builds the SDK and the page first): `regtest.ts` uses the mock wallet through the Unisat adapter for
  path B, path C and top-up (the seller's mock wallet locks first, the buyer pays and claims); `demo-page.ts` opens the demo page
  in playwright-core's Chromium and goes through path B (run `npx playwright-core install --only-shell chromium` first). They are
  not part of `npm test`; the input and output JSON are described in each file's header comment.
