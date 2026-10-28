#!/usr/bin/env bash
# Soak test of SPEC §15: the 3-node devnet and the indexer run continuously while path-B and path-C operations are
# submitted at random (ksana-cli; the signature scheme is random too: BIP340 or BIP322; path A has been removed,
# D-078),
# continuously checking:
#   - every operation is included (`--wait` waits for fast_confirmed or failed; path C is force-included after the BTC
#     confirmation);
#   - the three nodes have the same blocks at the settled heights;
#   - the indexer has not halted, and the block hash and state_root at every height it checked equal the node's.
# It also counts the parallel blocks merged by the checked main-chain blocks (D-063), showing that merged execution in
# the DAG really happened.
# With KSANA_SOAK_REORGS_PER_HOUR, BTC is also reorged at random times, 1 to 3 blocks deep (invalidateblock, then a
# longer branch), and after each reorg finality must pass the fast-lane tip of that moment on the three nodes and the
# indexer within 15 minutes: the blocks anchored on the orphaned BTC blocks leave the main chain (D-104). Their ops are
# included again in the new BTC order, where a path C op can fail out of turn (§7.4); an op signed on the account's
# nonce before the nodes retreated can then wait behind the nonce gap for good, and the soak test fills the gap with
# path B ops and checks that the op is included then, or that the entry pool dropped or refused it and its nonce was
# filled too.
#
#   devnet/soak.sh                              # build the image, start the devnet, run for 5 hours (D-061), clean up
#   KSANA_SOAK_SECONDS=1800 devnet/soak.sh      # run for 30 minutes
#   KSANA_SOAK_REORGS_PER_HOUR=4 devnet/soak.sh # with about 4 BTC reorgs an hour
#   KSANA_SMOKE_SKIP_BUILD=1 devnet/soak.sh     # use the existing ksana-node:devnet image
#   KSANA_SOAK_KEEP=1 devnet/soak.sh            # keep the devnet afterwards (no down -v)
#
# Without compose, connecting to nodes that are already running (e.g. binaries run directly on this machine):
#   KSANA_SOAK_EXTERNAL=1 KSANA_CLI="target/debug/ksana-cli" KSANA_ALICE_KEY=/path/alice.key \
#   KSANA_NODE_RPCS="http://127.0.0.1:18545 http://127.0.0.1:18546 http://127.0.0.1:18547" \
#   KSANA_INDEXER_RPC=http://127.0.0.1:18548 KSANA_BTC_WALLET=<bitcoind wallet> devnet/soak.sh
set -euo pipefail

cd "$(dirname "$0")/.."
duration=${KSANA_SOAK_SECONDS:-18000}
reorgs_per_hour=${KSANA_SOAK_REORGS_PER_HOUR:-0}
btc_url=${KSANA_BTC_RPC_URL:-http://127.0.0.1:18443}
btc_auth=${KSANA_BTC_RPC_AUTH:-ksana:ksana}
btc_wallet=${KSANA_BTC_WALLET:-devnet}
indexer_rpc=${KSANA_INDEXER_RPC:-http://127.0.0.1:18548}
read -r -a node_rpcs <<<"${KSANA_NODE_RPCS:-http://127.0.0.1:18545 http://127.0.0.1:18546 http://127.0.0.1:18547}"

if [ -z "${KSANA_SOAK_EXTERNAL:-}" ]; then
    compose=(docker compose -f devnet/docker-compose.yml --profile nodes)
    cleanup() {
        local status=$?
        if [ "$status" -ne 0 ]; then
            "${compose[@]}" logs --tail 200 || true
        fi
        if [ -z "${KSANA_SOAK_KEEP:-}" ]; then
            "${compose[@]}" down -v || true
        fi
        exit "$status"
    }
    trap cleanup EXIT
    if [ -z "${KSANA_SMOKE_SKIP_BUILD:-}" ]; then
        "${compose[@]}" build
    fi
    "${compose[@]}" up -d
    # ksana-cli inside the container: reaches the nodes and bitcoind via the compose network; the secret key is in
    # the devnet-config volume
    alice_key=/devnet/alice.key
    cli_btc=(--btc-rpc-url http://bitcoind:18443 --btc-wallet "$btc_wallet")
    cli() {
        local node=$1
        shift
        "${compose[@]}" run --rm --no-deps -T --entrypoint ksana-cli init --rpc "http://node$node:8545" "$@"
    }
else
    alice_key=${KSANA_ALICE_KEY:?KSANA_ALICE_KEY}
    read -r -a cli_cmd <<<"${KSANA_CLI:-ksana-cli}"
    cli_btc=(--btc-rpc-url "$btc_url" --btc-wallet "$btc_wallet")
    cli() {
        local node=$1
        shift
        "${cli_cmd[@]}" --rpc "${node_rpcs[$((node - 1))]}" "$@"
    }
fi

# JSON-RPC: jrpc <url> <method> <params JSON array>, prints result
jrpc() {
    curl -sf -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" "$1" | jq -c '.result'
}
# bitcoind: btc <method> <params> [wallet]
btc() {
    local url=$btc_url
    if [ $# -ge 3 ]; then
        url="$btc_url/wallet/$3"
    fi
    curl -sf --user "$btc_auth" -H 'content-type: text/plain' \
        --data-binary "{\"jsonrpc\":\"1.0\",\"id\":\"soak\",\"method\":\"$1\",\"params\":$2}" "$url" | jq -c '.result'
}

deadline=$((SECONDS + duration))
wait_for() { # wait_for <description> <timeout seconds> <command...>
    local what=$1 limit=$((SECONDS + $2))
    shift 2
    until "$@"; do
        if [ "$SECONDS" -ge "$limit" ]; then
            echo "timed out waiting for ${what}" >&2
            return 1
        fi
        sleep 2
    done
}

nodes_up() {
    local url
    for url in "${node_rpcs[@]}"; do
        jrpc "$url" ksana_getBlock '["latest"]' >/dev/null || return 1
    done
    jrpc "$indexer_rpc" indexer_status '[]' >/dev/null
}
wait_for "the nodes and the indexer to start" 900 nodes_up

alice=$(cli 1 key "$alice_key" | jq -r '.account')
bob=$(printf 'b%.0s' $(seq 64))
checked=0
# The parallel blocks merged by the main-chain block at each checked height (heights are checked again after a BTC reorg)
declare -A merged_at=()
declare -A count=([B]=0 [C]=0 [failed]=0 [left]=0 [gone]=0)
reorgs=0
# After a BTC reorg: the fast-lane height finality must pass, and by when
final_target=
final_deadline=0

# The seconds until the next BTC reorg: on average 3600 / KSANA_SOAK_REORGS_PER_HOUR, uniformly from half to one and a
# half times that
reorg_gap() {
    local mean=$((3600 / reorgs_per_hour))
    echo $((mean / 2 + RANDOM % (mean + 1)))
}
if [ "$reorgs_per_hour" -gt 0 ]; then
    next_reorg=$((SECONDS + $(reorg_gap)))
fi

# A BTC reorg k blocks deep (k from 1 to 3): invalidates the block k below the tip's successor and mines k + 1 blocks on
# the new branch (the miner service keeps mining on it); finality must then pass the fast-lane tip of this moment
btc_reorg() {
    local k tip hash addr fin
    k=$((RANDOM % 3 + 1))
    tip=$(btc getblockcount '[]')
    hash=$(btc getblockhash "[$((tip - k + 1))]")
    final_target=$(jrpc "${node_rpcs[0]}" ksana_getChainInfo '[]' | jq -r '.tip.height')
    final_deadline=$((SECONDS + 900))
    # the blocks above the finalized tip may leave the main chain: check those heights again
    fin=$(jrpc "${node_rpcs[0]}" ksana_getChainInfo '[]' | jq -r '.finalized.height')
    if [ "$fin" -lt "$checked" ]; then
        checked=$fin
    fi
    btc invalidateblock "[$hash]" >/dev/null
    addr=$(btc getnewaddress '[]' "$btc_wallet")
    btc generatetoaddress "[$((k + 1)), $addr]" >/dev/null
    reorgs=$((reorgs + 1))
    echo "$(date -u +%H:%M:%S) BTC reorg $reorgs: $k blocks from height $((tip - k + 1)); finality must pass fast-lane height $final_target"
}

# For wait_for at the end: finality has passed the last BTC reorg (a check that fails stops the soak test)
check_finality_done() {
    check_finality || exit 1
    [ -z "$final_target" ]
}

# After a BTC reorg: whether finality on the three nodes and the indexer has passed the fast-lane tip of the reorg's
# moment; fails once the deadline has passed without it
check_finality() {
    local url lowest height
    [ -n "$final_target" ] || return 0
    lowest=$(jrpc "$indexer_rpc" indexer_status '[]' | jq -r '.finalized.height')
    for url in "${node_rpcs[@]}"; do
        height=$(jrpc "$url" ksana_getChainInfo '[]' | jq -r '.finalized.height')
        if [ "$height" -lt "$lowest" ]; then
            lowest=$height
        fi
    done
    if [ "$lowest" -gt "$final_target" ]; then
        echo "$(date -u +%H:%M:%S) finality passed fast-lane height $final_target after BTC reorg $reorgs (now at $lowest)"
        final_target=
    elif [ "$SECONDS" -ge "$final_deadline" ]; then
        echo "finality has not passed fast-lane height $final_target within 15 minutes of BTC reorg $reorgs (at $lowest)" >&2
        return 1
    fi
}

# The account's nonce on each of the three nodes, one per line
nonces() {
    local url
    for url in "${node_rpcs[@]}"; do
        jrpc "$url" ksana_getAccount "[\"$alice\"]" | jq -r '.nonce'
    done
}
# The account's next nonce: the highest on the three nodes. An op goes to one of them at random, which may not have
# the block with the previous op yet: it keeps the op behind the nonce gap until it does. Signed on that node's own
# nonce, the op would take the previous op's nonce and be dropped once the block arrived
next_nonce() {
    nonces | sort -n | tail -n 1
}
nonce_past() { # nonce_past <nonce>: the account's next nonce is above it
    [ "$(next_nonce)" -gt "$1" ]
}

# An op signed while a BTC reorg was being settled, on the nonce of the main chain the nodes were leaving, can be left
# behind: it waits behind a nonce gap for good when the reorg made a path C op below it fail out of turn (§7.4, D-104),
# together with the path B ops the reorg took off that came back through the entry pool above the gap. It is dropped
# when an op included again takes its nonce, and by the entry pool, which keeps at most 8 of an account's ops behind a
# nonce gap (D-098), when the retreat returned that many above such a path C op, or above one whose native transaction
# has not confirmed again on the new branch (the nonce of a path C op waiting to be included again is no gap only when
# no path B op of the account comes before it, D-104); a node refuses it then, too.
# Fill the gap with path B ops one nonce at a time from the account's nonce up to the op's own: the ops waiting above a
# fill come in with it; a fill refused because one of them holds or took its nonce, or because the node still counts too
# many ops behind a gap, is tried again once that nonce has passed or after a while; the fill at the op's own nonce is
# refused while the op waits in the pool and goes in when the pool dropped or refused it
fill_gap() { # fill_gap <the nonce of the op left behind>
    local n out filled=() limit=$((SECONDS + 900)) retry_at
    while n=$(next_nonce) && [ "$n" -le "$1" ]; do
        if [ "$SECONDS" -ge "$limit" ]; then
            echo "timed out filling the nonce gap below the op left behind at nonce $1 (the account's nonce on the" \
                "three nodes: $(nonces | paste -sd /))" >&2
            return 1
        fi
        if out=$(cli 1 transfer --key "$alice_key" --to "$bob" --amount 1 --fee-token 1 --nonce "$n" --wait \
            --timeout 300); then
            if [ "$(jq -r '.status.status' <<<"$out")" != fast_confirmed ]; then
                echo "filling nonce $n for the op left behind at nonce $1: $out" >&2
                return 1
            fi
            filled+=("$n")
        else
            retry_at=$((SECONDS + 30))
            while ! nonce_past "$n" && [ "$SECONDS" -lt "$retry_at" ]; do
                sleep 2
            done
        fi
    done
    wait_for "the account's nonce to pass the op left behind at nonce $1" 300 nonce_past "$1"
    if [ "${#filled[@]}" -eq 0 ]; then
        echo "$(date -u +%H:%M:%S) the account's nonce passed the op left behind at nonce $1 without a fill"
    elif [ "${filled[-1]}" = "$1" ]; then
        count[gone]=$((count[gone] + 1))
        echo "$(date -u +%H:%M:%S) the entry pool had dropped the op left behind at nonce $1, or refused it: nonces ${filled[*]} were filled"
    else
        echo "$(date -u +%H:%M:%S) the op left behind at nonce $1 was included once nonces ${filled[*]} were filled"
    fi
}

# Check: the indexer has not halted; every height it newly checked is the same block on the three nodes with the same
# state_root; record the parallel blocks merged by these blocks. While a BTC reorg is being settled, the indexer and a
# node can be on either side of the retreat for a moment: a height that differs then is checked again next time
check() {
    local status height h ix block node url
    status=$(jrpc "$indexer_rpc" indexer_status '[]')
    if [ "$(jq -r '.halted' <<<"$status")" != null ]; then
        echo "indexer halted: $status" >&2
        return 1
    fi
    height=$(jq -r '.height' <<<"$status")
    for h in $(seq $((checked + 1)) "$height"); do
        ix=$(jrpc "$indexer_rpc" indexer_stateRoot "[\"$h\"]" | jq -c '{hash, state_root}')
        for url in "${node_rpcs[@]}"; do
            block=$(jrpc "$url" ksana_getBlock "[\"$h\"]")
            node=$(jq -c '{hash, state_root: .header.state_root}' <<<"$block")
            if [ "$node" != "$ix" ]; then
                if [ -n "$final_target" ]; then
                    checked=$((h - 1))
                    return 0
                fi
                echo "height $h: $url $node, indexer $ix" >&2
                return 1
            fi
        done
        merged_at[$h]=$(jq '.merged | length' <<<"$block")
    done
    checked=$height
}

# The parallel blocks merged by the main-chain blocks checked so far
merged() {
    local total=0 h
    for h in "${!merged_at[@]}"; do
        if [ "$h" -le "$checked" ]; then
            total=$((total + merged_at[$h]))
        fi
    done
    echo "$total"
}

sigs=(bip340 bip322)
while [ "$SECONDS" -lt "$deadline" ]; do
    node=$((RANDOM % 3 + 1))
    sig=${sigs[$((RANDOM % 2))]}
    # kept to find the op if a BTC reorg leaves it behind
    nonce=$(next_nonce)
    reorging=$final_target
    # path B to path C about 2 : 1 (path C has to wait for the BTC confirmation)
    case $((RANDOM % 3)) in
        0 | 1)
            path=B
            pay=(--fee-token 1)
            ;;
        *)
            path=C
            pay=(--native "${cli_btc[@]}")
            ;;
    esac
    if out=$(cli "$node" transfer --key "$alice_key" --to "$bob" --amount 1 "${pay[@]}" --nonce "$nonce" --sig "$sig" \
        --wait --timeout 300); then
        status=$(jq -r '.status.status' <<<"$out")
    elif [ -n "$reorging" ]; then
        status=left_behind
    else
        echo "operation at nonce $nonce was not submitted or not included (path $path, node $node; the account's nonce" \
            "on the three nodes now: $(nonces | paste -sd /))" >&2
        exit 1
    fi
    case $status in
        fast_confirmed | btc_final) count[$path]=$((count[$path] + 1)) ;;
        failed) count[failed]=$((count[failed] + 1)) ;;
        left_behind)
            count[left]=$((count[left] + 1))
            fill_gap "$nonce"
            ;;
        *)
            echo "operation was not included (path $path): $out" >&2
            exit 1
            ;;
    esac
    check
    check_finality
    echo "$(date -u +%H:%M:%S) path $path ($sig, node $node) $status; total B ${count[B]} / C ${count[C]} / failed ${count[failed]} / left behind ${count[left]} (dropped ${count[gone]}); indexer checked up to height $checked ($(merged) parallel blocks merged)"
    if [ "$reorgs_per_hour" -gt 0 ] && [ -z "$final_target" ] && [ "$SECONDS" -ge "$next_reorg" ]; then
        btc_reorg
        next_reorg=$((SECONDS + $(reorg_gap)))
    fi
done
wait_for "finality to pass the last BTC reorg" 900 check_finality_done
check
echo "soak test passed: $duration seconds, ${count[B]} path-B and ${count[C]} path-C operations (${count[failed]} failed, ${count[left]} left behind by BTC reorgs, ${count[gone]} of them dropped or refused by the entry pool), $reorgs BTC reorgs, indexer checked up to height $checked, whose main-chain blocks merged $(merged) parallel blocks"
