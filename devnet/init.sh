#!/bin/bash
# Initialization and BTC block mining for the Ksana devnet (D-042), run in the compose nodes profile.
#
#   ksana-devnet init    Run once after bitcoind starts; writes to the shared directory $KSANA_DEVNET_DIR
#                        (default /devnet):
#                          genesis.json              genesis config: anchored at the BTC tip at that time, timestamp =
#                                                    the current time, KSA pre-allocated to alice
#                          node<i>/funding.key       each node's settlement funding secret key; 4 confirmed 1 BTC UTXOs
#                                                    are sent to the funding address
#                          node<i>/producer.key/.pub producer secret key and public key (the --producer argument)
#                          alice.key                 secret key of the test account alice with pre-allocated KSA
#                          share.key                 secret key of the block share account (D-089): 20% of every
#                                                    reward slot is credited to it
#                        Does nothing when already initialized (genesis.json exists).
#   ksana-devnet miner   Mines one BTC block every $BTC_BLOCK_INTERVAL seconds (default 10) so that settlement and
#                        native transactions get confirmed.
set -euo pipefail

OUT=${KSANA_DEVNET_DIR:-/devnet}
RPC=${BTC_RPC_URL:-http://bitcoind:18443}
AUTH=${BTC_RPC_AUTH:-ksana:ksana}
NODES=${KSANA_NODES:-3}
WALLET=devnet

# bitcoind JSON-RPC: rpc <method> <params (JSON array or object by name)> [wallet]; prints result (JSON), on error
# prints the reason and returns 1.
rpc() {
    local url=$RPC body
    if [ $# -ge 3 ]; then
        url="$RPC/wallet/$3"
    fi
    body=$(curl -sS --user "$AUTH" -H 'content-type: application/json' \
        --data-binary "{\"jsonrpc\":\"1.0\",\"id\":\"devnet\",\"method\":\"$1\",\"params\":$2}" "$url")
    if [ "$(jq -c '.error' <<<"$body")" != "null" ]; then
        echo "bitcoind $1 failed: $(jq -c '.error' <<<"$body" || echo "$body")" >&2
        return 1
    fi
    jq -c '.result' <<<"$body"
}

# Strips the quotes of a JSON string.
unquote() {
    jq -r '.'
}

wallet_address() {
    rpc createwallet "[\"$WALLET\"]" >/dev/null 2>&1 \
        || rpc loadwallet "[\"$WALLET\"]" >/dev/null 2>&1 \
        || true
    rpc getnewaddress '[]' "$WALLET" | unquote
}

init() {
    if [ -f "$OUT/genesis.json" ]; then
        echo "devnet already initialized: $OUT/genesis.json"
        return 0
    fi
    mkdir -p "$OUT"
    addr=$(wallet_address)
    height=$(rpc getblockcount '[]')
    if [ "$height" -lt 101 ]; then
        # a coinbase needs 100 confirmations before it can be spent
        rpc generatetoaddress "[$((101 - height)), \"$addr\"]" >/dev/null
    fi
    i=1
    while [ "$i" -le "$NODES" ]; do
        mkdir -p "$OUT/node$i"
        funding=$(ksana-keytool "$OUT/node$i/funding.key" | jq -r '.regtest_address')
        for _ in 1 2 3 4; do
            # the regtest wallet has no fee estimation, give the fee rate explicitly
            rpc sendtoaddress "{\"address\":\"$funding\",\"amount\":1.0,\"fee_rate\":2}" "$WALLET" >/dev/null
        done
        ksana-keytool "$OUT/node$i/producer.key" | jq -r '.account' >"$OUT/node$i/producer.pub"
        echo "node$i: funding address $funding, producer $(cat "$OUT/node$i/producer.pub")"
        i=$((i + 1))
    done
    alice=$(ksana-keytool "$OUT/alice.key" | jq -r '.account')
    share=$(ksana-keytool "$OUT/share.key" | jq -r '.account')
    rpc generatetoaddress "[1, \"$addr\"]" >/dev/null
    height=$(rpc getblockcount '[]')
    # the BTC block hash in the genesis config is in internal byte order (D-001); the RPC returns display byte order
    hash=$(rpc getblockhash "[$height]" | unquote | fold -w2 | tac | tr -d '\n')
    cat >"$OUT/genesis.json.tmp" <<JSON
{
  "timestamp": "$(date +%s)",
  "btc_anchor": { "height": $height, "hash": "$hash" },
  "allocations": [ { "account": "$alice", "amount": "1000000000000" } ],
  "block_share_account": "$share"
}
JSON
    mv "$OUT/genesis.json.tmp" "$OUT/genesis.json"
    echo "genesis config written to $OUT/genesis.json (anchored at BTC height $height), alice = $alice, block share account = $share"
}

miner() {
    addr=$(wallet_address)
    interval=${BTC_BLOCK_INTERVAL:-10}
    while true; do
        sleep "$interval"
        rpc generatetoaddress "[1, \"$addr\"]" >/dev/null || echo "mining a BTC block failed, retrying later"
    done
}

case "${1:-init}" in
    init) init ;;
    miner) miner ;;
    *)
        echo "usage: ksana-devnet [init|miner]" >&2
        exit 2
        ;;
esac
