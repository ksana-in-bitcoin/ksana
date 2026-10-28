# Joining the public signet (D-107)

Ksana's public test network runs on **Bitcoin's public signet** (chain ID 1002, `ANCHOR_DEPTH` 1, D-056). Its genesis config
is `signet/genesis.json` in this repository, and the project hosts a seed node, an indexer, the web wallet and a KSA faucet
on one server (`signet/README.md` is the operator's guide) at `signet.ksana.network` (D-112). Its former name
still answers `/rpc`, `/indexer`, `/btc` and `/faucet`, and its page moves to the new name. The
contract playground at `playground.ksana.network` (D-119), example contracts to use, edit, compile and deploy from the
browser, runs on the same signet: its page calls the same server, where its compile service runs.

Everything here is test money. Signet BTC (sBTC) comes free from any signet faucet; signet KSA comes from the genesis
allocation through the faucet below and from block rewards. A consensus change before mainnet restarts the signet from
genesis (a new `signet/genesis.json`): every balance starts over then.

| What | Where |
| --- | --- |
| Web wallet | `https://signet.ksana.network/` |
| Node JSON-RPC (`docs/RPC.md`) | `https://signet.ksana.network/rpc`, WebSocket `wss://signet.ksana.network/rpc` |
| Indexer (`indexer_status`, `indexer_stateRoot`) | `https://signet.ksana.network/indexer` |
| Bitcoin RPC for the wallet's path C and top-ups: `getblockcount`, `getblockchaininfo`, `estimatesmartfee`, `sendrawtransaction` (no `scantxoutset`); and two `GET` paths of mempool.space's signet API for listing an address's coins, `esplora/block-height/0` and `esplora/address/<tb1p…>/utxo` | `https://signet.ksana.network/btc` |
| KSA faucet: `GET` its state, `POST {"to": "<account>"}` for 100 KSA once a day per account | `https://signet.ksana.network/faucet` |
| Contract playground (`playground/README.md`) | `https://playground.ksana.network/` |
| The playground's compile service, which builds an example's edited source (`playground/compiler/README.md`) | `https://signet.ksana.network/compile` |
| Seed node P2P | `/dns4/signet.ksana.network/tcp/9000` |
| Genesis config | `signet/genesis.json` |

## 1. With a browser wallet only

1. Install Unisat or Xverse (D-052), switch it to **Signet**, and use a **Taproot (P2TR)** address: the account is the
   Taproot address (D-092), so other address types cannot hold KSA. In Xverse that is the ordinals address.
2. Open `https://signet.ksana.network/`. A first visit opens on the signet's node, indexer and Bitcoin RPC (the page's
   `deployment.json`); the data source panel switches to another node or to the simulator.
3. Wallet → Connect wallet. Wallet → Receive → "Get test KSA from the signet faucet" sends 100 KSA to the connected account.
4. Wallet → Send, **path B** (the default): the fee is paid in KSA (the page suggests one), the wallet signs one BIP322
   message (the VmOp's sighash, no PSBT), and the transfer is fast-confirmed within seconds. The page then counts its
   settlement's signet confirmations to 6, about an hour after the transfer, and shows it BTC-final once the signet block
   that decided it has 6 confirmations too (D-114, D-116), about 110 minutes after the transfer.
5. **Path C** (Send → More ways to submit): the op travels in a Bitcoin transaction that the wallet signs as a PSBT, so the
   Taproot address needs a little sBTC (a few thousand sats, from any signet faucet). The page lists the address's
   outputs through mempool.space's signet API and broadcasts through `https://signet.ksana.network/btc`; while mempool.space
   does not answer, the page says so and the listing can be tried again. A producer must include the op after its BTC
   confirmations (forced inclusion, D-036), with no KSA fee.
6. **Top-up** (Wallet → Top up, D-085): buys KSA with sBTC from a lock that a seller made for this account in an exchange
   contract (`contracts/examples/ksa-exchange`); the seller's side is `ksana-cli exchange lock`.

Each step shows the op's status on its page; Operations and Blocks show where it landed on Bitcoin.

Contracts take the same wallet: `https://playground.ksana.network/` deploys and calls the example contracts (a token, an
NFT collection, an AMM, a lending market, a multisig wallet, vesting) with path B operations signed in it, and compiles
edited sources. A contract's page in the web wallet's explorer opens it there.

## 2. bitcoind

Every node and indexer reads Bitcoin through its own bitcoind on the public signet, with `-txindex`:

```sh
bitcoind -signet -server -txindex=1 -rpcuser=ksana -rpcpassword=<password> -rpcbind=127.0.0.1 -rpcallowip=127.0.0.1
bitcoin-cli -signet -rpcuser=ksana -rpcpassword=<password> getblockchaininfo   # wait until initialblockdownload is false
```

The RPC port is 38332. The first sync takes a few hours and a few tens of GB of disk. Bitcoin Core 30 or later relays path
C's large `OP_RETURN` by default, which matters when this bitcoind broadcasts path C transactions (the CLI's `--native`
and `top-up`); earlier versions need `-datacarriersize=100000`. Settlement and republish payloads travel in a reveal
transaction's witness (D-081), which every version relays.

## 3. A node of your own

Ubuntu 22.04 with the Rust toolchain of `rust-toolchain.toml` (on Linux the build also needs `make` and `clang`, D-103):

```sh
git clone https://github.com/ksana-in-bitcoin/Ksana && cd Ksana
cargo build --release -p ksana-node -p ksana-indexer -p ksana-cli --bins
target/release/ksana-node --chain signet --data-dir ~/ksana-signet/node --genesis signet/genesis.json \
    --btc-rpc-url http://127.0.0.1:38332 --btc-rpc-user ksana --btc-rpc-pass <password> \
    --p2p-listen /ip4/0.0.0.0/tcp/9000 --peer /dns4/signet.ksana.network/tcp/9000
```

`KSANA_BTC_RPC_PASS=<password>` in the environment can replace `--btc-rpc-pass`, which keeps the password off the process
list (the node, the indexer and the probe read it).

The node syncs the chain from the seed node and checks every block against signet (anchors, settlements, forced
inclusion, BTC-anchored finality). Its RPC is `http://127.0.0.1:8545` (`--rpc-addr`; `--rpc-cors '*'` lets a browser page
call it, D-055): point the web wallet's data source panel or `ksana-cli --rpc` at it. Open TCP port 9000 so that others can
connect to you as well.

The indexer rebuilds the chain from signet alone, without talking to any node, and checks every block's `state_root`
(D-048):

```sh
target/release/ksana-indexer --chain signet --data-dir ~/ksana-signet/indexer --genesis signet/genesis.json \
    --btc-rpc-url http://127.0.0.1:38332 --btc-rpc-user ksana --btc-rpc-pass <password>   # RPC 127.0.0.1:8546
```

Compare `indexer_stateRoot(<height>)` with the node's block at that height, as `devnet/smoke.sh` does.

## 4. CPU mining

Any node mines when it is given a payee account:

```sh
target/release/ksana-cli --chain signet key miner.key   # prints the account and its tb1p… address; or use your wallet's Taproot address
target/release/ksana-node ... --producer <account or tb1p… address> [--btc-funding-key-file funding.key --btc-funding-wallet ksana-funding]
```

The proof of work is Argon2id with 2 MiB of memory, one evaluation taking about 1 ms on one core (D-094); the node mines on
one thread. ASERT keeps the network at about one block every 5 seconds while there are ops (D-026); with nothing to include
a block is empty and takes 12 to 120 times the work, from about a minute up to ten while the network stays idle (two-speed
difficulty, D-084, D-090). The blocks you mine pay their rewards to your account (SPEC §8.6): each block's reward slots,
less the block share (D-089) and the carrier's part when someone else lands its data on Bitcoin (D-095), plus the path B
fees it includes. A reward is unlocked, and appears in your balance, once the data of the block and of every block in its
past has landed on Bitcoin (SPEC §8.5); `ksana-cli --chain signet account <account>` shows what is still locked.

- **Without** `--btc-funding-key-file` your blocks still include path B ops; another producer's settlement carries their
  data to Bitcoin and takes the carrier reward (SPEC §7.5).
- **With** it, your node settles its own blocks: `target/release/ksana-keytool funding.key` creates the key and prints
  its funding address (`signet_address`); send that address at least three outputs of sBTC and let them confirm (0.006
  sBTC in all is enough to start). Settlements pay the floor `--settlement-feerate` (default 2 sat/vB) while signet's
  mempool holds at most half a block, and bitcoind's fee estimate up to `--max-settlement-feerate` when it holds more; a
  pair left waiting two blocks is bumped to the estimate, or to twice its own rate and more while the mempool has room,
  up to the same cap (D-102, D-117). Keep the floor at 2 although signet's minimum is 1 sat/vB: on 2026-10-13 signet's
  blocks took nothing below 2 while a backlog at 1 sat/vB waited (D-107). Cap it at 5 like the seed nodes
  (`--max-settlement-feerate 5`): bitcoind's estimate can stay far above what signet's blocks take; from 2026-10-16 it
  stayed at 100.03 sat/vB, and before D-117 every pair paid the default cap of 100, about 37 000 sats (D-107). What it
  costs: a settlement pair is about 320 to 380 vB plus a 240-sat anchor output, so about 1 000 sats at 2 sat/vB; even an
  idle network sends one about every two signet blocks, paid by whichever producer made the block carrying it (D-109),
  so a node producing a third of the blocks spends about 0.00025 sBTC a day at 2 sat/vB, up to 0.0005 with traffic: 0.02
  sBTC lasts it about six weeks. Every settlement returns its change to the address, and several can be in flight, one
  per output. Top up below 0.002 sBTC or three outputs (`bitcoin-cli -signet -rpcwallet=ksana-funding listunspent 0`
  with the wallet below): a node without funds keeps producing, but its blocks' data waits for another producer to carry
  it.
- Finding the funding outputs: a node with a funding key on signet must use a watch-only wallet of your bitcoind that
  holds the funding address (`--btc-funding-wallet`, D-109), which answers at once, and refuses to start without one. A
  scan of signet's whole UTXO set with `scantxoutset` took 49 seconds cold and 31 warm on 2026-10-13 (height 326 207,
  about 80 million outputs), close to the node's 60-second RPC timeout, and the producer would wait for one on every
  signet block. Make the wallet before the funding arrives:

  ```sh
  bitcoin-cli -signet -named createwallet wallet_name=ksana-funding disable_private_keys=true blank=true load_on_startup=true
  bitcoin-cli -signet validateaddress <funding address>               # its scriptPubKey
  bitcoin-cli -signet getdescriptorinfo "raw(<scriptPubKey>)"         # the descriptor with its checksum
  bitcoin-cli -signet -rpcwallet=ksana-funding importdescriptors '[{"desc": "<descriptor>", "timestamp": "now"}]'
  target/release/ksana-node ... --btc-funding-key-file funding.key --btc-funding-wallet ksana-funding
  ```

  When the funding confirmed before the import, the wallet does not see it: rescan from a signet height before it
  arrived (`bitcoin-cli -signet -rpcwallet=ksana-funding rescanblockchain <height>`), or import with that time as
  `"timestamp"` instead of `"now"`.

## 5. ksana-cli

```sh
export KSANA_RPC=https://signet.ksana.network/rpc   # or your own node
ksana-cli --chain signet key alice.key        # account and tb1p… address
ksana-cli --chain signet account @alice.key   # nonce, balances, locked fees and rewards
ksana-cli --chain signet transfer --key alice.key --to <account> --amount 5 --fee-token 1 --wait   # path B
ksana-cli --chain signet transfer --key alice.key --to <account> --amount 5 --native --btc-wallet <wallet> \
    --btc-rpc-url http://127.0.0.1:38332 --btc-rpc-user ksana --btc-rpc-pass <password> --wait     # path C
ksana-cli --chain signet op <op_id>           # fast_confirmed, then btc_final
ksana-cli --chain signet settlement <height>  # the commit / reveal pair on signet
```

`--amount` and the fees are in KSA base units (10^8 per KSA, D-066). Path C and `top-up` fund and broadcast through a
bitcoind wallet holding sBTC (`bitcoin-cli -signet createwallet <wallet>`, then send it sBTC).

## 6. Testing with real wallets

The acceptance of the public signet (M32) includes paths B and C from Unisat and from Xverse, from the hosted page. For
each, a report names the wallet and its version, the browser, the path, the `op_id`, the block, and the Bitcoin
transaction that carried it (the settlement's reveal txid for path B, the native transaction for path C).
`ksana-cli --chain signet op <op_id>` and `settlement <height>` give them, as do the page's Operations and Blocks.

Anything that misbehaves (a wallet refusing to sign, a transaction signet's miners leave out, a settlement that waits)
goes into an issue on GitHub, with the time and the txids: the signet runs for at least four weeks before mainnet to observe
`SETTLEMENT_TIMEOUT` and the carrier reward on a real fee market (SPEC §14).
