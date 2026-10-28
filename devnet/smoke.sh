#!/usr/bin/env bash
# Smoke test of the devnet (D-042, D-048, the CI devnet job): builds the image, starts bitcoind, init, btc-miner, the
# 3 fast-lane nodes and the indexer, waits for the nodes to produce blocks and converge on the same chain (the blocks
# on the common prefix come from more than one producer, showing that the nodes are connected and accept each
# other's blocks); then submits a path-B transfer as alice with ksana-cli, drives an asset with an issuer capability
# (D-121: a transfer to a frozen account fails its op, and goes through once he is unfrozen), waits for the last op's
# block to enter the settled prefix, and checks that the indexer's block hash and state_root at every checked height
# equal the node's. Then cleans up.
#
#   devnet/smoke.sh                        # build the image and run
#   KSANA_SMOKE_SKIP_BUILD=1 devnet/smoke.sh   # use the existing ksana-node:devnet image
set -euo pipefail

cd "$(dirname "$0")/.."
compose=(docker compose -f devnet/docker-compose.yml --profile nodes)
ports=(18545 18546 18547)
target=${KSANA_SMOKE_HEIGHT:-12}
timeout=${KSANA_SMOKE_TIMEOUT:-600}

cleanup() {
    local status=$?
    if [ "$status" -ne 0 ]; then
        "${compose[@]}" logs --tail 200 || true
    fi
    "${compose[@]}" down -v || true
    exit "$status"
}
trap cleanup EXIT

if [ -z "${KSANA_SMOKE_SKIP_BUILD:-}" ]; then
    "${compose[@]}" build
fi
"${compose[@]}" up -d

# ksana_getBlock: block <port> <block id>, prints result (JSON)
block() {
    curl -sf -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ksana_getBlock\",\"params\":[\"$2\"]}" \
        "http://127.0.0.1:$1" | jq -c '.result'
}

height() {
    block "$1" latest | jq -r '.header.height'
}

deadline=$((SECONDS + timeout))
until_ok() { # until_ok <description> <command...>: retries until the command succeeds or the timeout expires
    local what=$1
    shift
    until "$@"; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            echo "timed out waiting for ${what}" >&2
            return 1
        fi
        sleep 2
    done
}

all_reached() {
    local p h
    for p in "${ports[@]}"; do
        h=$(height "$p" 2>/dev/null) || return 1
        [ "${h:-0}" -ge "$target" ] || return 1
    done
}

check=$((target - 3))
same_block() {
    local n
    n=$(for p in "${ports[@]}"; do block "$p" "$check" | jq -r '.hash'; done | sort -u | wc -l)
    [ "$n" -eq 1 ]
}

until_ok "all nodes to reach height $target" all_reached
until_ok "all nodes to agree at height $check" same_block
producers=$(for h in $(seq 1 "$check"); do block "${ports[0]}" "$h" | jq -r '.header.producer'; done | sort -u | wc -l)
echo "node heights: $(for p in "${ports[@]}"; do height "$p"; done | tr '\n' ' '); $producers producers at heights 1..$check"
if [ "$producers" -lt 2 ]; then
    echo "the blocks on the common prefix come from a single producer" >&2
    exit 1
fi

# ksana-cli <arguments...> runs in the compose network (alice's secret key is in the devnet-config volume), with the
# fixed module of the test contract probe at /probe.wasm
cli() {
    "${compose[@]}" run --rm --no-deps -T -v "$PWD/crates/ksana-execution/tests/data/probe.wasm:/probe.wasm:ro" \
        --entrypoint ksana-cli init --rpc http://node1:8545 "$@"
}
# rpc <method> <params JSON array>: node1's result
rpc() {
    curl -sf -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" \
        "http://127.0.0.1:${ports[0]}" | jq -c '.result'
}

# ksana-cli submits a path-B transfer as alice
bob=$(printf 'b%.0s' $(seq 64))
op=$(cli transfer --key /devnet/alice.key --to "$bob" --amount 5 --fee-token 1 --wait --timeout 120)
# block.height is the height of the main-chain block that accepted it (a merged block takes effect at the height of
# the main-chain block merging it, D-063); the settlement is queried by the block that included it (block.hash)
op_height=$(jq -r '.status.block.height' <<<"$op")
op_block=$(jq -r '.status.block.hash' <<<"$op")
echo "transfer submitted by ksana-cli is in block $op_block (accepted at height $op_height): $(jq -c '{op_id, status: .status.status}' <<<"$op")"

# An asset with an issuer capability (D-121): alice deploys probe, whose methods anyone may call; it creates an asset
# with the freeze capability, issued to alice, and freezes bob, so alice's transfer to him fails its op, and once he is
# unfrozen it goes through. The checks below cover the blocks of these ops too.
check_op() { # check_op <--wait result> [error]: the op succeeded, or failed with that error
    if [ -z "${2:-}" ]; then
        jq -e '.status.receipt.success == true' >/dev/null <<<"$1" && return
    else
        jq -e --arg e "$2" '.status.receipt.success == false and .status.receipt.error == $e' >/dev/null <<<"$1" && return
    fi
    echo "unexpected op result: $1" >&2
    return 1
}
contract_id() { # contract_id <account> <nonce> <module>: sha256(account ‖ nonce as u64 little-endian ‖ sha256(module))
    python3 -c 'import hashlib, sys
code = hashlib.sha256(open(sys.argv[3], "rb").read()).digest()
seed = bytes.fromhex(sys.argv[1]) + int(sys.argv[2]).to_bytes(8, "little") + code
print(hashlib.sha256(seed).hexdigest())' "$@"
}
call() { # call <method> <Borsh arguments in hex>: alice calls probe
    cli call --key /devnet/alice.key --contract "$probe" --method "$1" --args "$2" --max-gas 5000000 --fee-token 1 \
        --wait --timeout 120
}
send() { # send <amount>: alice transfers the asset to bob
    cli transfer --key /devnet/alice.key --to "$bob" --asset "$asset" --amount "$1" --fee-token 1 --wait --timeout 120
}
alice=$(jq -r '.account' <<<"$op")
# init's argument: the Borsh bytes of b"ok"
deployed=$(cli deploy --key /devnet/alice.key --code /probe.wasm --init-args 020000006f6b --max-gas 5000000 \
    --fee-token 1 --wait --timeout 120)
check_op "$deployed"
probe=$(contract_id "$alice" "$(jq -r '.nonce' <<<"$deployed")" crates/ksana-execution/tests/data/probe.wasm)
sub_id=$(printf '07%.0s' $(seq 32))
asset=$(python3 -c 'import hashlib, sys; print(hashlib.sha256(bytes.fromhex(sys.argv[1])).hexdigest())' "$probe$sub_id")
# create_asset(sub_id, { name: "Dollar", symbol: "USDX", decimals: 6 }, CAP_FREEZE, alice, 1 000)
meta=06000000446f6c6c6172040000005553445806
check_op "$(call create_asset "$sub_id${meta}02${alice}e8030000000000000000000000000000")"
# freeze(sub_id, bob, true)
check_op "$(call freeze "$sub_id${bob}01")"
check_op "$(send 5)" "rejected: recipient is frozen for this asset"
standing=$(rpc ksana_getAssetStatus "[\"$asset\",\"$bob\"]")
if ! jq -e '.status == ["frozen"] and .can_receive == false' >/dev/null <<<"$standing"; then
    echo "bob's standing in the asset: $standing" >&2
    exit 1
fi
check_op "$(call freeze "$sub_id${bob}00")"
op=$(send 5)
check_op "$op"
record=$(rpc ksana_getAsset "[\"$asset\"]")
if ! jq -e '.symbol == "USDX" and .total_supply == "1000" and .capabilities == ["freeze"] and .paused == false' \
    >/dev/null <<<"$record"; then
    echo "the asset: $record" >&2
    exit 1
fi
op_height=$(jq -r '.status.block.height' <<<"$op")
op_block=$(jq -r '.status.block.hash' <<<"$op")
echo "asset $asset with the freeze capability: a transfer to a frozen account failed its op, the transfer after" \
    "unfreezing is in block $op_block (accepted at height $op_height)"
# the waits below get the whole timeout again
deadline=$((SECONDS + timeout))

settled() {
    curl -sf -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ksana_getSettlement\",\"params\":[\"$op_block\"]}" \
        "http://127.0.0.1:${ports[0]}" | jq -e '.result.settled == true' >/dev/null
}
# indexer <method> <params JSON array>, prints result
indexer() {
    curl -sf -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" \
        http://127.0.0.1:18548 | jq -c '.result'
}
indexed() {
    local h
    h=$(indexer indexer_status '[]' | jq -r '.height') || return 1
    [ "${h:-0}" -ge "$op_height" ]
}
until_ok "the transfer's block to enter the settled prefix" settled
until_ok "the indexer to check up to height $op_height" indexed
status=$(indexer indexer_status '[]')
if [ "$(jq -r '.halted' <<<"$status")" != null ]; then
    echo "indexer halted: $status" >&2
    exit 1
fi
indexed_height=$(jq -r '.height' <<<"$status")
for h in $(seq 1 "$indexed_height"); do
    node=$(block "${ports[0]}" "$h" | jq -c '{hash, state_root: .header.state_root}')
    ix=$(indexer indexer_stateRoot "[\"$h\"]" | jq -c '{hash, state_root}')
    if [ "$node" != "$ix" ]; then
        echo "height $h: node $node, indexer $ix" >&2
        exit 1
    fi
done
echo "indexer checked up to height $indexed_height; the block hash and state_root at every height match the node"
echo "devnet smoke test passed"
