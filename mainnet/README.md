# Mainnet genesis config template (D-096)

`genesis.json` is the genesis config template for mainnet and **cannot start a network as it is**: `block_share_account` is the placeholder
`KSANA_BLOCK_SHARE_ACCOUNT_TBD___` (hex `4b53414e…5f5f5f`), and `timestamp` and `btc_anchor` are zero values. The node and the indexer check
the file when they build the genesis block (`GenesisFile::check_network`, `crates/ksana-execution/src/genesis_file.rs`): a genesis config that
carries the placeholder is rejected on every network; `--chain mainnet` additionally requires the block share account to be given, no genesis
allocations, and the anchor and the timestamp to be filled in. Mainnet does not start at all yet, whatever the file holds: its block share is not
published (D-134).

After the consensus freeze (SPEC v2 final, D-096) the only open item is the receiving wallet of the block share, the project owner's decision.
The mainnet parameters are `MAINNET` in `crates/ksana-primitives/src/params.rs` (the mainnet column of SPEC §13; the rationale for each value
is in SPEC §13.1), where the block share's fraction and end hold placeholders until they are published (D-134). The project owner confirmed on
2026-10-06 the six values D-096 had chosen as defaults (SPEC §17.2).

## The four fields to fill in before starting mainnet

1. `block_share_account`: the block share account, the hex of the 32-byte output key of a Taproot address (the `account` printed by
   `ksana-cli key <private key file>` or `ksana-keytool`, D-092). The block share of every reward slot is credited to it (D-089).
   It is written into the genesis state (state key 0x0B) and enters the config fingerprint, so it cannot be changed after launch.
2. `timestamp`: the genesis time (Unix seconds), the time at which the nodes are started. ASERT uses the genesis block as its anchor (D-026); a
   timestamp far earlier than the actual block production time keeps the difficulty at the lower bound for a long time.
3. `btc_anchor`: the BTC block at genesis, the height and hash of a block at least `ANCHOR_DEPTH` (2 on mainnet) blocks below the tip, the hash
   in internal byte order (D-001):
   ```sh
   height=$(( $(bitcoin-cli getblockcount) - 2 ))
   hash=$(bitcoin-cli getblockhash "$height" | fold -w2 | tac | tr -d '\n')
   ```
4. `bits` (optional, default = the PoW limit `0x1f00ffff`, decimal 520093695): the genesis difficulty. At the limit about 13 CPU cores already
   produce a block every 5 seconds, and the difficulty can only adjust upward from the genesis value (D-027); when the hashrate at launch is far
   above that, set `bits` near the expected hashrate to avoid an initial period of too-fast blocks.

`allocations` must stay empty: on mainnet the upper bound on total rewards is the max supply of 210 million KSA itself, which leaves no room for
a genesis allocation (D-089).

## Starting

```sh
ksana-node --chain mainnet --data-dir <dir> --genesis mainnet/genesis.json --producer <account> \
    --btc-rpc-url http://127.0.0.1:8332 --btc-funding-key-file funding.key --btc-funding-wallet ksana-funding
ksana-indexer --chain mainnet --data-dir <dir> --genesis mainnet/genesis.json --btc-rpc-url http://127.0.0.1:8332
```

All nodes and indexers must use the same filled-in config (the config fingerprint is written into the data directory). bitcoind needs
`-txindex=1`; settlement and republish payloads live in the witness of the reveal transaction (D-081), and path C native transactions carry an
`OP_RETURN` data output (relayed by default since Bitcoin Core 30). A node with a funding key lists its funding outputs from a watch-only wallet
of the same bitcoind that holds the funding address (`--btc-funding-wallet`, D-109) and refuses to start without one: a scan of the UTXO set
already takes 30 to 50 seconds on signet. `docs/SIGNET.md` §4 shows how to make the wallet (the same commands without `-signet`), before the
funding arrives.
