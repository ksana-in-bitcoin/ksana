#!/bin/bash
# Operator commands of the public signet (D-107), run inside the ksana image by signet/docker-compose.yml and
# signet/deploy.sh. The keys and the genesis config live in $KSANA_SIGNET_DIR (default /signet, the host directory
# $KSANA_SIGNET_HOME of signet/.env):
#
#   keys/node1/funding.key  producer.key  (node2 the same)   settlement funding and producer keys of the seed nodes
#   keys/faucet.key  keys/operator.key                        accounts of the genesis allocations
#   keys/share.key                                            the block share account of the signet (D-089)
#   keys/probe.key                                            the relay probe's funds
#   keys/keeper.key  keys/seller.key                          the price keeper of the official lending market and the
#                                                             top-up seller of the testnet points (D-126)
#   genesis.json                                              the genesis config (committed as signet/genesis.json)
#
#   ksana-signet keys                    create the missing keys; print the accounts and the funding addresses; when
#                                        bitcoind answers, also the wallets below
#   ksana-signet genesis [--force]       write genesis.json anchored at the current signet tip (needs a synced bitcoind)
#   ksana-signet wait-btc                wait until bitcoind has finished its initial block download
#   ksana-signet node <name> [args...]   wait for bitcoind, then run the seed node <name> (node1 or node2)
#   ksana-signet indexer [args...]       wait for bitcoind, then run the indexer
#   ksana-signet status                  bitcoind, the nodes with their funding (from their wallets), the indexer and
#                                        the faucet
#   ksana-signet probe [args...]         the relay probe (crates/ksana-btc/examples/relay_probe.rs) with keys/probe.key
#                                        and the wallet ksana-probe (its fee rate: --feerate, default auto)
#
# The seed nodes and the probe find their coins through watch-only wallets of bitcoind, ksana-node1, ksana-node2 and
# ksana-probe, instead of scanning signet's whole UTXO set, which takes about as long as bitcoind's RPC client in the
# node waits (D-109). Each holds raw(<locking script>) of one address and is created when missing, by keys, by the
# node or the probe before it starts, and by status; an existing wallet is kept as it is. bitcoind's
# password reaches the binaries through the environment (KSANA_BTC_RPC_PASS) and curl through standard input, never
# through a command line.
#
# Genesis settings (environment): KSANA_SIGNET_PRODUCERS (1 or 2, the seed nodes that mine; picks the default bits),
# KSANA_SIGNET_BITS (the genesis difficulty, overrides the default), KSANA_SIGNET_FAUCET_KSA (default 100000000) and
# KSANA_SIGNET_OPERATOR_KSA (default 10000000), the KSA the faucet and the operator accounts receive at genesis.
set -euo pipefail

DIR=${KSANA_SIGNET_DIR:-/signet}
KEYS=$DIR/keys
GENESIS=$DIR/genesis.json
BTC_URL=${BTC_RPC_URL:-http://bitcoind:38332}
BTC_USER=${BTC_RPC_USER:-ksana}
BTC_PASS=${BTC_RPC_PASS:-}
# 10^8 base units per KSA (D-066)
KSA_UNIT=100000000
# The genesis difficulty when nobody sets KSANA_SIGNET_BITS: an Argon2id evaluation takes about 1 ms on one core
# (D-094), and every seed node mines on one thread, so for one block per 5 seconds a block takes about 5 000
# evaluations per producer: 2^256 / 5 000 is 0x1f0d1b71, 2^256 / 10 000 is 0x1f068db8 (ASERT corrects from there, D-026)
BITS_ONE_PRODUCER=520952689
BITS_TWO_PRODUCERS=520523192
# A seed node's funding (D-109): status sets top_up below TOP_UP_SATS or three outputs (TOP_UP_SATS is about six days
# of half an idle network's settlement pairs at the seed nodes' floor of 2 sat/vB), and warns when no seed node holds
# PAIR_SATS confirmed, about the most one pair needs at 2 sat/vB (its fees, the 240-sat P2A output and the change)
TOP_UP_SATS=200000
PAIR_SATS=1400

log() {
    echo "[ksana-signet] $*" >&2
}

# bitcoind JSON-RPC: rpc <method> <params as a JSON array>; prints the result (JSON), on error prints the reason
# and returns 1. The credentials go to curl on standard input.
rpc() {
    call_btc "$BTC_URL" "$@"
}

# wallet_rpc <wallet> <method> <params>: the same at the endpoint of one of bitcoind's wallets.
wallet_rpc() {
    local wallet=$1
    shift
    call_btc "$BTC_URL/wallet/$wallet" "$@"
}

call_btc() {
    local body
    body=$(curl -sS --max-time 600 -K - -H 'content-type: application/json' \
        --data-binary "{\"jsonrpc\":\"1.0\",\"id\":\"signet\",\"method\":\"$2\",\"params\":$3}" "$1" \
        <<<"user = \"$BTC_USER:$BTC_PASS\"") || return 1
    if [ "$(jq -c '.error' <<<"$body")" != "null" ]; then
        log "bitcoind $2 failed: $(jq -c '.error' <<<"$body" 2>/dev/null || echo "$body")"
        return 1
    fi
    jq -c '.result' <<<"$body"
}

# Ksana JSON-RPC: ksana <url> <method> <params as a JSON array>; prints the result (JSON).
ksana() {
    curl -sf --max-time 10 -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" "$1" | jq -c '.result'
}

keys() {
    local n k
    mkdir -p "$KEYS/node1" "$KEYS/node2"
    chmod 700 "$KEYS"
    for n in node1 node2; do
        ksana-keytool "$KEYS/$n/funding.key" | jq -r '.signet_address' >"$KEYS/$n/funding.address"
        ksana-keytool "$KEYS/$n/producer.key" | jq -r '.account' >"$KEYS/$n/producer.pub"
    done
    for k in faucet operator share probe keeper seller; do
        ksana-keytool "$KEYS/$k.key" | jq -r '.account' >"$KEYS/$k.pub"
    done
    # The faucet (signet/services/faucet.mjs), the keeper (server/keeper.ts of ksana-points) and the points service
    # with the seller (server/seller.ts of ksana-points) run as the node user (uid 1000) of their images and read their keys through a
    # bind mount of the one file
    chown 1000:1000 "$KEYS/faucet.key" "$KEYS/keeper.key" "$KEYS/seller.key"
    jq -n \
        --arg node1_funding "$(cat "$KEYS/node1/funding.address")" \
        --arg node1_producer "$(cat "$KEYS/node1/producer.pub")" \
        --arg node2_funding "$(cat "$KEYS/node2/funding.address")" \
        --arg node2_producer "$(cat "$KEYS/node2/producer.pub")" \
        --arg faucet "$(cat "$KEYS/faucet.pub")" \
        --arg operator "$(cat "$KEYS/operator.pub")" \
        --arg share "$(cat "$KEYS/share.pub")" \
        --arg probe "$(ksana-keytool "$KEYS/probe.key" | jq -r '.signet_address')" \
        --arg keeper "$(cat "$KEYS/keeper.pub")" \
        --arg seller "$(cat "$KEYS/seller.pub")" \
        '{node1: {funding_address: $node1_funding, producer: $node1_producer},
          node2: {funding_address: $node2_funding, producer: $node2_producer},
          faucet: $faucet, operator: $operator, block_share: $share, probe_address: $probe, keeper: $keeper,
          seller: $seller}'
}

# ensure_wallet <wallet> <address> <key file>: bitcoind's watch-only wallet <wallet> watching <address> (see the top
# of this file). A missing wallet is created blank and without private keys and loaded at every start of bitcoind;
# the descriptor raw(<locking script>) is imported with the key file's time as its birth, so that bitcoind looks
# for the address's coins in the blocks since the key was made (also the blocks a syncing bitcoind connects later).
# A wallet that already watches the address is left alone.
ensure_wallet() {
    local wallet=$1 address=$2 born script desc result
    born=$(stat -c %Y "$3")
    if ! rpc listwallets '[]' | jq -e --arg w "$wallet" 'any(.[]; . == $w)' >/dev/null; then
        # an existing wallet that is not loaded is loaded (and kept loaded at bitcoind's next start), else created
        if ! rpc loadwallet "[\"$wallet\", true]" >/dev/null 2>&1; then
            rpc createwallet "[\"$wallet\", true, true, \"\", false, true, true]" >/dev/null || return 1
            log "created bitcoind's watch-only wallet $wallet"
        fi
    fi
    if wallet_rpc "$wallet" getaddressinfo "[\"$address\"]" 2>/dev/null | jq -e '.ismine' >/dev/null; then
        return 0
    fi
    script=$(rpc validateaddress "[\"$address\"]" | jq -r '.scriptPubKey // empty')
    [ -n "$script" ] || return 1
    desc=$(rpc getdescriptorinfo "[\"raw($script)\"]" | jq -r '.descriptor') || return 1
    # a rescan from the key's birth: bitcoind answers once it is done (the one parameter is the array of requests)
    result=$(wallet_rpc "$wallet" importdescriptors "[[{\"desc\": \"$desc\", \"timestamp\": $born}]]") || return 1
    if ! jq -e 'all(.success)' <<<"$result" >/dev/null; then
        log "importing $desc into wallet $wallet failed: $result"
        return 1
    fi
    log "wallet $wallet watches $address, looking from $(date -u -d "@$born" +%FT%TZ) on"
}

# The wallets of the seed nodes and the probe, when bitcoind answers (keys); otherwise each is made when it is needed.
wallets() {
    local n
    if ! rpc getblockcount '[]' >/dev/null 2>&1; then
        log "bitcoind is not running yet: each wallet is made when its node or the probe starts"
        return 0
    fi
    for n in node1 node2; do
        ensure_wallet "ksana-$n" "$(cat "$KEYS/$n/funding.address")" "$KEYS/$n/funding.key" || true
    done
    ensure_wallet ksana-probe "$(ksana-keytool "$KEYS/probe.key" | jq -r '.signet_address')" "$KEYS/probe.key" || true
}

# Whether bitcoind has finished its initial block download and has the blocks of all the headers it knows.
btc_synced() {
    local info
    info=$(rpc getblockchaininfo '[]' 2>/dev/null) || return 1
    [ "$(jq -r '.initialblockdownload' <<<"$info")" = false ] &&
        [ "$(jq -r '.headers - .blocks' <<<"$info")" -le 1 ]
}

wait_btc() {
    local waited=0 info
    until btc_synced; do
        if [ $((waited % 60)) -eq 0 ]; then
            if info=$(rpc getblockchaininfo '[]' 2>/dev/null); then
                log "waiting for bitcoind: $(jq -r '"\(.chain) block \(.blocks) of \(.headers), progress \(.verificationprogress * 1000 | floor / 10)%"' <<<"$info")"
            else
                log "waiting for bitcoind at $BTC_URL"
            fi
        fi
        sleep 10
        waited=$((waited + 10))
    done
}

# The genesis config (D-027): anchored one block below the signet tip (ANCHOR_DEPTH is 1 on signet, D-056), the
# timestamp is now (ASERT's anchor, D-026), the faucet and operator accounts receive their KSA and the signet's own
# block share account receives the block share (D-089).
genesis() {
    if [ -f "$GENESIS" ] && [ "${1:-}" != "--force" ]; then
        log "$GENESIS already exists (pass --force to replace it, which starts a new network)"
        return 1
    fi
    if [ "$(rpc getblockchaininfo '[]' | jq -r '.chain')" != signet ]; then
        log "bitcoind is not on signet"
        return 1
    fi
    wait_btc
    keys >/dev/null
    local tip height hash bits faucet_ksa operator_ksa
    tip=$(rpc getblockcount '[]')
    # ANCHOR_DEPTH (1 on signet) blocks below the tip, like mainnet's genesis anchor (mainnet/README.md): the blocks
    # anchor there or above, and a one-block signet reorg cannot take the genesis anchor away
    height=$((tip - 1))
    # the BTC block hash in the genesis config is in internal byte order (D-001); the RPC returns display byte order
    hash=$(rpc getblockhash "[$height]" | jq -r '.' | fold -w2 | tac | tr -d '\n')
    case "${KSANA_SIGNET_PRODUCERS:-1}" in
        1) bits=$BITS_ONE_PRODUCER ;;
        2) bits=$BITS_TWO_PRODUCERS ;;
        *)
            log "KSANA_SIGNET_PRODUCERS must be 1 or 2"
            return 1
            ;;
    esac
    bits=${KSANA_SIGNET_BITS:-$bits}
    faucet_ksa=${KSANA_SIGNET_FAUCET_KSA:-100000000}
    operator_ksa=${KSANA_SIGNET_OPERATOR_KSA:-10000000}
    jq -n \
        --arg timestamp "$(date +%s)" \
        --argjson height "$height" \
        --arg hash "$hash" \
        --argjson bits "$bits" \
        --arg faucet "$(cat "$KEYS/faucet.pub")" \
        --arg faucet_amount "$((faucet_ksa * KSA_UNIT))" \
        --arg operator "$(cat "$KEYS/operator.pub")" \
        --arg operator_amount "$((operator_ksa * KSA_UNIT))" \
        --arg share "$(cat "$KEYS/share.pub")" \
        '{timestamp: $timestamp, btc_anchor: {height: $height, hash: $hash}, bits: $bits,
          allocations: [{account: $faucet, amount: $faucet_amount}, {account: $operator, amount: $operator_amount}],
          block_share_account: $share}' >"$GENESIS.tmp"
    mv "$GENESIS.tmp" "$GENESIS"
    log "genesis config written to $GENESIS: anchored at signet height $height, bits $bits"
    cat "$GENESIS"
}

require_genesis() {
    if [ ! -f "$GENESIS" ]; then
        log "no genesis config at $GENESIS: run signet/deploy.sh <host> genesis first"
        exit 1
    fi
}

# A seed node. The genesis difficulty already matches the seed nodes' hashrate, so blocks come about every 5 seconds
# from the start; --min-block-interval-ms only bounds the burst after an outage, while ASERT has lowered the difficulty
# (D-026). The settlement fee rate floor is the node's default, 2 sat/vB, and not signet's MIN_SETTLEMENT_FEERATE of 1:
# while a backlog of transactions at 1 sat/vB waited on 2026-10-13, signet's blocks took nothing below 2 (D-107).
# The cap is 5 sat/vB and not the node's default of 100: from 2026-10-16 bitcoind's estimate stayed at 100.03 sat/vB,
# the rate of the seed nodes' own settlements, while signet's blocks took transactions at 1 sat/vB, so every
# settlement pair paid the cap, about 37 000 sats instead of about 1 000 (D-107). Since D-117 the nodes pay the floor
# while signet's mempool holds at most half a block, and the cap bounds what the estimate costs when it holds more.
# The funding outputs come from the node's wallet (--btc-funding-wallet), and the node never splits them
# (--funding-pool-target 0): the operator funds the address with several outputs (D-109).
node() {
    local name=${1:?usage: ksana-signet node <node1|node2> [args...]}
    shift
    require_genesis
    if [ ! -f "$KEYS/$name/producer.pub" ] || [ ! -f "$KEYS/$name/funding.key" ]; then
        log "no keys for $name: run ksana-signet keys first"
        exit 1
    fi
    wait_btc
    until ensure_wallet "ksana-$name" "$(cat "$KEYS/$name/funding.address")" "$KEYS/$name/funding.key"; do
        log "making the wallet ksana-$name failed; trying again in 30 s"
        sleep 30
    done
    export KSANA_BTC_RPC_PASS=$BTC_PASS
    exec ksana-node --chain signet --data-dir /data --genesis "$GENESIS" \
        --rpc-addr 0.0.0.0:8545 --rpc-cors '*' \
        --btc-rpc-url "$BTC_URL" --btc-rpc-user "$BTC_USER" \
        --producer "$(cat "$KEYS/$name/producer.pub")" --btc-funding-key-file "$KEYS/$name/funding.key" \
        --btc-funding-wallet "ksana-$name" --funding-pool-target 0 \
        --p2p-listen /ip4/0.0.0.0/tcp/9000 --min-block-interval-ms 1000 \
        --settlement-feerate 2 --max-settlement-feerate 5 "$@"
}

indexer() {
    require_genesis
    wait_btc
    export KSANA_BTC_RPC_PASS=$BTC_PASS
    exec ksana-indexer --chain signet --data-dir /data --genesis "$GENESIS" \
        --btc-rpc-url "$BTC_URL" --btc-rpc-user "$BTC_USER" --rpc-addr 0.0.0.0:8546 "$@"
}

status() {
    local info n nodes chain faucet totals address result total=0 listed=0 payers=0
    if info=$(rpc getblockchaininfo '[]'); then
        jq -c '{bitcoind: {chain, blocks, headers, initialblockdownload, verificationprogress, size_on_disk}}' <<<"$info"
    else
        echo '{"bitcoind": "unreachable"}'
    fi
    # bitcoind's peer traffic since it started, and the upload cap (-maxuploadtarget, per day)
    if totals=$(rpc getnettotals '[]'); then
        jq -c '{bitcoind_traffic: {received_mb: (.totalbytesrecv / 1e6 | floor), sent_mb: (.totalbytessent / 1e6 | floor),
            upload_cap_mb: (.uploadtarget.target / 1e6 | floor), upload_left_today_mb: (.uploadtarget.bytes_left_in_cycle / 1e6 | floor),
            upload_cap_reached: .uploadtarget.target_reached}}' <<<"$totals"
    fi
    nodes=node1
    [ "${KSANA_SIGNET_PRODUCERS:-1}" = 2 ] && nodes="node1 node2"
    for n in $nodes; do
        if chain=$(ksana "http://$n:8545" ksana_getChainInfo '[]' 2>/dev/null) && [ -n "$chain" ]; then
            jq -c --arg n "$n" '{($n): {chain_id, tip: .tip.height, daa_score, finalized: .finalized.height}}' <<<"$chain"
        else
            jq -cn --arg n "$n" '{($n): "not running"}'
        fi
        # The node's funding as its wallet lists it (D-109): the node settles with the confirmed outputs and never
        # splits them; pending is the change of its settlements still in the mempool
        total=$((total + 1))
        if [ -f "$KEYS/$n/funding.address" ]; then
            address=$(cat "$KEYS/$n/funding.address")
            if ensure_wallet "ksana-$n" "$address" "$KEYS/$n/funding.key" &&
                result=$(wallet_rpc "ksana-$n" listunspent '[0]'); then
                result=$(jq -c --arg a "$address" --argjson low "$TOP_UP_SATS" '
                    (map(select(.confirmations > 0) | .amount * 100000000 | round) | add // 0) as $confirmed
                    | (map(select(.confirmations == 0) | .amount * 100000000 | round) | add // 0) as $pending
                    | {funding_address: $a, confirmed_sats: $confirmed, pending_sats: $pending, outputs: length,
                       top_up: ($confirmed + $pending < $low or length < 3)}' <<<"$result")
                jq -c --arg n "$n" '{($n): .}' <<<"$result"
                listed=$((listed + 1))
                if [ "$(jq '.confirmed_sats' <<<"$result")" -ge "$PAIR_SATS" ]; then
                    payers=$((payers + 1))
                fi
            fi
        fi
    done
    if [ "$listed" = "$total" ] && [ "$payers" = 0 ]; then
        echo '{"warning": "no seed node can pay for a settlement pair: blocks keep coming, but nothing lands on signet, so BTC finality and the indexer stand still until a seed node is funded (D-109)"}'
    fi
    if chain=$(ksana http://indexer:8546 indexer_status '[]' 2>/dev/null) && [ -n "$chain" ]; then
        jq -c '{indexer: .}' <<<"$chain"
    else
        echo '{"indexer": "not running"}'
    fi
    if [ -f "$KEYS/faucet.pub" ] && faucet=$(ksana http://node1:8545 ksana_getAccount "[\"$(cat "$KEYS/faucet.pub")\"]" 2>/dev/null) && [ -n "$faucet" ]; then
        jq -c '{faucet: {account, nonce, balances}}' <<<"$faucet"
    fi
}

probe() {
    keys >/dev/null
    ensure_wallet ksana-probe "$(ksana-keytool "$KEYS/probe.key" | jq -r '.signet_address')" "$KEYS/probe.key" ||
        exit 1
    export KSANA_BTC_RPC_PASS=$BTC_PASS
    # The fee rate is the probe's default, auto: bitcoind's estimate for 2 blocks, at least 2 sat/vB, plus 1
    exec ksana-relay-probe --key "$KEYS/probe.key" --rpc-url "$BTC_URL" --rpc-user "$BTC_USER" --wallet ksana-probe \
        --network signet "$@"
}

if [ -z "$BTC_PASS" ]; then
    log "BTC_RPC_PASS is not set"
    exit 2
fi
command=${1:-}
shift || true
case "$command" in
    keys)
        keys
        wallets
        ;;
    genesis) genesis "$@" ;;
    wait-btc) wait_btc ;;
    node) node "$@" ;;
    indexer) indexer "$@" ;;
    status) status "$@" ;;
    probe) probe "$@" ;;
    *)
        echo "usage: ksana-signet keys | genesis [--force] | wait-btc | node <name> [args] | indexer [args] | status | probe [args]" >&2
        exit 2
        ;;
esac
