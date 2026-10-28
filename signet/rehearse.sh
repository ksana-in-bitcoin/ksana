#!/usr/bin/env bash
# Rehearsal of the hosted public signet (D-107) on this machine: builds the images, runs the whole stack of
# signet/docker-compose.yml with signet/rehearsal.yml (a private signet mined every 15 seconds instead of Bitcoin's
# public signet) and checks it through the page's origin, the way a visitor reaches it:
#
#   - the operator commands of signet/ksana-signet.sh: keys (with bitcoind's watch-only wallets), genesis, status;
#   - node1 lists its funding outputs from its wallet, produces blocks and settles them on BTC, and the indexer agrees
#     with the node (block hash and state_root at every height it has checked, as devnet/smoke.sh);
#   - Caddy serves the page and deployment.json and proxies /rpc, /indexer, /btc and /faucet;
#   - the page keeps out of other sites' frames, and the endpoints send no framing header;
#   - the BTC proxy answers the wallet's methods and refuses the others, and checks the Esplora paths' shapes;
#   - the faucet sends KSA, and it arrives;
#   - the contract playground's compile service (D-119) at /compile, which builds the counter's own source into the
#     module the playground ships and answers a broken source with the line of its error, and the origin of the
#     playground's page (a Cloudflare Worker, here the development server's), whose calls node1, the faucet and the
#     compile service allow;
#   - the testnet points service (D-126) at /points, which answers the points page's origin alone and credits the
#     faucet's transfer at BTC-final, here with a configuration naming the rehearsal's faucet; the season's official
#     instances, deployed with the operator's key as signet/deploy.sh <host> instances does, a second time without a
#     change; the price keeper, which then finds the market following the pool; and the top-up seller, which locks
#     KSA on the official exchange for a buyer who signed in, a lock ksana-cli pays from the rehearsal's wallet with
#     the claim in the same transaction (path C), and the points service credits the top-up at BTC-final;
#   - the relay probe's transactions are mined.
#
# The images are built from what signet/deploy.sh pushes (signet/sources.sh): the committed HEAD of this repository and
# of ksana-web and ksana-points beside it (KSANA_WEB_DIR and KSANA_POINTS_DIR name other checkouts).
#
#   signet/rehearse.sh
#   KSANA_REHEARSAL_SKIP_BUILD=1 signet/rehearse.sh   # use the existing ksana-signet:* images
#   KSANA_REHEARSAL_KEEP=1 signet/rehearse.sh         # leave the stack running on http://127.0.0.1:18080
set -euo pipefail

cd "$(dirname "$0")/.."
commits=$(signet/sources.sh commits --points)
home=$(mktemp -d)
port=${KSANA_REHEARSAL_PORT:-18080}
timeout=${KSANA_REHEARSAL_TIMEOUT:-2700}
base=http://127.0.0.1:$port
compose=(docker compose -p ksana-signet-rehearsal --project-directory signet -f signet/docker-compose.yml
    -f signet/rehearsal.yml --env-file "$home/.env" --profile playground --profile points)

pass=$(openssl rand -hex 24)
salt=$(openssl rand -hex 16)
hmac=$(printf '%s' "$pass" | openssl dgst -sha256 -hmac "$salt" | awk '{print $NF}')
cat >"$home/.env" <<EOF
KSANA_SIGNET_HOME=$home
BTC_RPC_PASS=$pass
BTC_RPCAUTH='ksana:$salt\$$hmac'
KSANA_SIGNET_DOMAIN=:80
KSANA_SIGNET_PRODUCERS=1
KSANA_FAUCET_KSA=100
KSANA_REHEARSAL_PORT=$port
KSANA_PLAYGROUND_ORIGIN=http://localhost:5174
KSANA_POINTS_ORIGIN=http://localhost:5175
$commits
EOF
mkdir -p "$home/config"

cleanup() {
    local status=$?
    if [ "$status" -ne 0 ]; then
        "${compose[@]}" logs --tail 100 || true
    fi
    if [ -n "${KSANA_REHEARSAL_KEEP:-}" ] && [ "$status" -eq 0 ]; then
        echo "the stack keeps running on $base; stop it with: (cd $PWD && ${compose[*]} down -v)"
        exit 0
    fi
    # the keys and the official instances' steps were written by root inside the containers
    "${compose[@]}" run --rm --no-deps --entrypoint rm ops -rf /signet/keys /signet/genesis.json /signet/points \
        >/dev/null 2>&1 || true
    "${compose[@]}" down -v --remove-orphans || true
    rm -rf "$home"
    exit "$status"
}
trap cleanup EXIT
signet/sources.sh dir "$home/src" --points
cd "$home/src"

if [ -z "${KSANA_REHEARSAL_SKIP_BUILD:-}" ]; then
    "${compose[@]}" build
fi

deadline=$((SECONDS + timeout))
until_ok() { # until_ok <description> <command...>: retries until the command succeeds or the timeout expires
    local what=$1
    shift
    until "$@"; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            echo "timed out waiting for ${what}" >&2
            return 1
        fi
        sleep 3
    done
}
# btc <bitcoin-cli arguments>: bitcoind of the private signet (RPC cookie)
btc() {
    "${compose[@]}" exec -T bitcoind bitcoin-cli -signet -datadir=/home/bitcoin/.bitcoin "$@"
}
# post <path> <JSON body>: a request through Caddy, prints the response body
post() {
    curl -sS --max-time 60 -H 'content-type: application/json' --data "$2" "$base$1"
}
ksana() { # ksana <method> <params JSON array>: node1's JSON-RPC through /rpc, prints result
    post /rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" | jq -c '.result'
}
indexer() {
    post /indexer "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" | jq -c '.result'
}

# mine <blocks>: signet's proof of work (the same limit on every signet) takes bitcoind a few million hashes a block,
# more than generatetoaddress tries by default
mine() {
    btc -rpcwallet=rehearsal generatetoaddress "$1" "$(btc -rpcwallet=rehearsal getnewaddress)" 1000000000 >/dev/null
}

"${compose[@]}" up -d --wait bitcoind
btc createwallet rehearsal >/dev/null
# a coinbase can be spent after 100 confirmations
mine 101

keys=$("${compose[@]}" run --rm --no-deps -T ops keys)
funding=$(jq -r '.node1.funding_address' <<<"$keys")
probe=$(jq -r '.probe_address' <<<"$keys")
# three outputs, as signet/README.md asks of the operator (D-109)
for _ in 1 2 3; do
    btc -rpcwallet=rehearsal sendtoaddress "$funding" 1 >/dev/null
done
btc -rpcwallet=rehearsal sendtoaddress "$probe" 0.01 >/dev/null
mine 1
echo "keys: $(jq -c '{node1, faucet}' <<<"$keys")"
# The points service's configuration (rehearsal.yml mounts it): the rehearsal's own faucet and operator, as
# signet/points.json of ksana-points names the public signet's
jq -n --arg faucet "$(jq -r '.faucet' <<<"$keys")" --arg operator "$(jq -r '.operator' <<<"$keys")" \
    '{chain_id: 1002, faucets: [$faucet], exclude: {($faucet): "the faucet", ($operator): "the operator"}, chains: {}}' \
    >"$home/points.json"

# The genesis difficulty of 250 Argon2id evaluations a block instead of 5 000: the private signet mines a block every
# 15 seconds, and SETTLEMENT_TIMEOUT (6 BTC blocks) must leave node1 time for the 2 blocks a settlement waits for
"${compose[@]}" run --rm -T -e KSANA_SIGNET_BITS=536938020 ops genesis | tail -1
"${compose[@]}" up -d btc-miner
"${compose[@]}" up -d

# The page and deployment.json
until_ok "the page at $base" curl -sf -o /dev/null "$base/"
# grep reads what curl wrote in full: grep -q at the end of a pipe can leave curl writing into a closed pipe (exit 23)
grep -q '<div id="root">' <<<"$(curl -sf "$base/")"
[ "$(curl -sf "$base/deployment.json" | jq -c .)" = "$(jq -c . signet/deployment.json)" ]
echo "page: $base/ serves the web wallet with deployment.json"
# The page keeps out of other sites' frames, and the endpoints' answers send neither header (curl -D -: the headers)
page_headers=$(curl -sS -o /dev/null -D - --max-time 30 "$base/" | tr -d '\r')
grep -qix "content-security-policy: frame-ancestors 'none'" <<<"$page_headers"
grep -qix 'x-frame-options: deny' <<<"$page_headers"
for path in /rpc /indexer /btc /faucet; do
    answer=$(curl -sS -o /dev/null -D - --max-time 30 "$base$path")
    if grep -qi -E '^(content-security-policy|x-frame-options):' <<<"$answer"; then
        echo "$path answered with the page's framing headers" >&2
        exit 1
    fi
done
echo "page: kept out of other sites' frames; /rpc, /indexer, /btc and /faucet send neither framing header"

# The contract playground's compile service: the counter's own source gives the module the playground ships, and a
# broken one an error on its line
until_ok "the compile service" sh -c "curl -sf --max-time 30 '$base/compile/health' | jq -e '.ok' >/dev/null"
curl -sSf --max-time 30 "$base/compile/health" | jq -e '.crates | index("playground-counter") and index("amm") and index("lending")' >/dev/null
compile() { # compile <crate> <source file>: the service's answer
    jq -n --arg crate "$1" --rawfile source "$2" '{crate: $crate, source: $source}' |
        curl -sSf --max-time 120 -H 'content-type: application/json' --data-binary @- "$base/compile"
}
built=$(compile playground-counter contracts/examples/playground-counter/src/lib.rs)
shipped=$(jq -r '."playground-counter".wasm' playground/src/examples/wasm/builds.json)
if [ "$(jq -r '.ok' <<<"$built")" != true ] || [ "$(jq -r '.sha256' <<<"$built")" != "$shipped" ]; then
    echo "the compile service built the counter's own source into $(jq -c '{ok, sha256, diagnostics}' <<<"$built"), not the" \
        "module the playground ships ($shipped)" >&2
    exit 1
fi
broken=$home/broken.rs
awk '/^fn change\(/ && !done { print "fn broken() -> u64 { \"text\" }"; done = 1 } { print }' \
    contracts/examples/playground-counter/src/lib.rs >"$broken"
line=$(grep -n '^fn broken()' "$broken" | cut -d: -f1)
compile playground-counter "$broken" | jq -e --argjson line "$line" '.ok == false and
    any(.diagnostics[]; .level == "error" and any(.spans[]; .is_primary and .line_start == $line))' >/dev/null
answer=$(curl -sS -o /dev/null -D - --max-time 30 "$base/compile/health")
if grep -qi -E '^(content-security-policy|x-frame-options):' <<<"$answer"; then
    echo "/compile answered with the page's framing headers" >&2
    exit 1
fi
echo "compile service: the counter's own source builds into the shipped module ($shipped), a broken one answers its line $line"

# node1 produces blocks (with no entries at the empty-block weight, D-084: about a minute each at first)
produced() {
    local h
    h=$(ksana ksana_getChainInfo '[]' | jq -r '.tip.height') || return 1
    [ "${h:-0}" -ge 1 ]
}
until_ok "node1 to produce a block" produced
[ "$(ksana ksana_getChainInfo '[]' | jq -r '.chain_id')" = 1002 ]
# node1 lists its funding outputs from its watch-only wallet, never scanning the UTXO set (the log is read whole:
# compose fails on the pipe a grep -q closes early)
node1_log=$("${compose[@]}" logs node1)
grep -q "listed from bitcoind's wallet ksana-node1" <<<"$node1_log"
btc -rpcwallet=ksana-node1 getwalletinfo | jq -e '.private_keys_enabled == false' >/dev/null
echo "funding: node1 lists its funding outputs from the watch-only wallet ksana-node1"

# The BTC proxy: four of the wallet's methods, every other one refused, scantxoutset too (coins come from Esplora)
count=$(post /btc '{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}' | jq -r '.result')
[ "$count" -gt 101 ]
post /btc '{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' | jq -e '.result.chain == "signet"' >/dev/null
post /btc '{"jsonrpc":"2.0","id":1,"method":"getpeerinfo","params":[]}' | jq -e '.error.code == -32601' >/dev/null
post /btc '{"jsonrpc":"2.0","id":1,"method":"scantxoutset","params":["start",["raw(51)"]]}' |
    jq -e '.error.code == -32601 and (.error.message | test("Esplora"))' >/dev/null
# The Esplora paths: mempool.space answers for Bitcoin's public signet, not this private one, so only the shapes the
# proxy refuses itself are checked; the genesis hash is an answer or 502 when mempool.space is out of reach
http_code() {
    curl -s -o /dev/null -w '%{http_code}' --max-time 30 "$base$1"
}
[ "$(http_code /btc/esplora/address/bc1qnotsignet/utxo)" = 400 ]
[ "$(http_code /btc/esplora/tx/00)" = 404 ]
case "$(http_code /btc/esplora/block-height/0)" in
    200 | 502) ;;
    *)
        echo "the BTC proxy's Esplora genesis path answered neither 200 nor 502" >&2
        exit 1
        ;;
esac
# The proxy's own headers, asked past Caddy, which adds nosniff to every answer itself: a refused Esplora path is plain
# text with nosniff, whatever an upstream answer said
headers=$("${compose[@]}" exec -T btc-proxy node -e "fetch('http://127.0.0.1:8080/esplora/tx/00').then((r) =>
    console.log(r.status, r.headers.get('content-type'), r.headers.get('x-content-type-options')))")
[ "$headers" = "404 text/plain nosniff" ]
echo "btc proxy: getblockcount $count, getblockchaininfo on signet, scantxoutset and other methods refused, Esplora shapes and headers checked"

# The faucet sends 100 KSA to a new account
until_ok "the faucet" sh -c "curl -sf '$base/faucet' | jq -e '.balance != \"0\"' >/dev/null"
to=$(printf 'c%.0s' $(seq 64))
reply=$(post /faucet "{\"to\":\"$to\"}")
op=$(jq -r '.op_id' <<<"$reply")
[ "${#op}" -eq 64 ] || {
    echo "the faucet refused: $reply" >&2
    exit 1
}
post /faucet "{\"to\":\"$to\"}" | jq -e '.error | test("already received")' >/dev/null
received() {
    ksana ksana_getAccount "[\"$to\"]" | jq -e '.balances[] | select(.amount == "10000000000")' >/dev/null
}
until_ok "the faucet's transfer to arrive" received
op_block=$(ksana ksana_getOpStatus "[\"$op\"]" | jq -r '.block.hash')
echo "faucet: 100 KSA arrived at $to (op $op)"

# The playground's page calls node1, the faucet and the compile service from its own origin, and a browser asks first
# for a JSON body: node1 allows any origin, the faucet and the compile service the page's alone
origin=$(sed -n 's/^KSANA_PLAYGROUND_ORIGIN=//p' "$home/.env")
allowed() { # allowed <origin> <path>: the origin that a preflight from <origin> is allowed, or nothing
    curl -sS -o /dev/null -D - --max-time 30 -X OPTIONS -H "Origin: $1" -H 'Access-Control-Request-Method: POST' \
        -H 'Access-Control-Request-Headers: content-type' "$base$2" | tr -d '\r' |
        grep -i '^access-control-allow-origin:' | sed 's/^[^:]*: *//'
}
for path in /rpc /faucet /compile; do
    expected=$origin
    [ "$path" != /rpc ] || expected='*'
    if [ "$(allowed "$origin" "$path")" != "$expected" ]; then
        echo "$path does not allow the playground's page at $origin" >&2
        exit 1
    fi
    if [ "$path" != /rpc ] && [ "$(allowed https://elsewhere.example "$path")" = https://elsewhere.example ]; then
        echo "$path allows a page of another origin" >&2
        exit 1
    fi
done
curl -sSf -o /dev/null -D - --max-time 30 -H "Origin: $origin" "$base/faucet" | tr -d '\r' |
    grep -qix "access-control-allow-origin: $origin"
echo "playground: node1 allows any origin, the faucet and the compile service the playground's page at $origin alone"

# Settlement on BTC, and the indexer agrees with the node
settled() {
    ksana ksana_getSettlement "[\"$op_block\"]" | jq -e '.settled == true' >/dev/null
}
until_ok "the faucet transfer's block to be settled on BTC" settled
op_height=$(ksana ksana_getOpStatus "[\"$op\"]" | jq -r '.block.height')
indexed() {
    local h
    h=$(indexer indexer_status '[]' | jq -r '.height') || return 1
    [ "${h:-0}" -ge "$op_height" ]
}
until_ok "the indexer to check up to height $op_height" indexed
status=$(indexer indexer_status '[]')
if [ "$(jq -r '.halted' <<<"$status")" != null ]; then
    echo "indexer halted: $status" >&2
    exit 1
fi
for h in $(seq 1 "$(jq -r '.height' <<<"$status")"); do
    node=$(ksana ksana_getBlock "[\"$h\"]" | jq -c '{hash, state_root: .header.state_root}')
    ix=$(indexer indexer_stateRoot "[\"$h\"]" | jq -c '{hash, state_root}')
    if [ "$node" != "$ix" ]; then
        echo "height $h: node $node, indexer $ix" >&2
        exit 1
    fi
done
echo "settlement: the faucet transfer's block is settled; the indexer agrees with node1 up to height $(jq -r '.height' <<<"$status")"

# The points service (D-126): it follows node1's chain, answers the points page's origin and no other, and credits the
# faucet's transfer to the faucet task once its block is BTC-final, while the account, with no X account linked, does not
# take part (D-127)
until_ok "the points service" sh -c "curl -sf --max-time 30 '$base/points/health' >/dev/null"
genesis=$(ksana ksana_getBlock '["0"]' | jq -r '.hash')
curl -sSf --max-time 30 "$base/points/status" | jq -e --arg g "$genesis" '.chain_id == 1002 and .genesis == $g' >/dev/null
points_origin=$(sed -n 's/^KSANA_POINTS_ORIGIN=//p' "$home/.env")
if [ "$(allowed "$points_origin" /points/auth/challenge)" != "$points_origin" ] ||
    [ "$(allowed https://elsewhere.example /points/auth/challenge)" = https://elsewhere.example ]; then
    echo "/points does not allow the points page at $points_origin alone" >&2
    exit 1
fi
answer=$(curl -sS -o /dev/null -D - --max-time 30 "$base/points/status")
if grep -qi -E '^(content-security-policy|x-frame-options):' <<<"$answer"; then
    echo "/points answered with the page's framing headers" >&2
    exit 1
fi
faucet_credited() {
    curl -sf --max-time 30 "$base/points/account/$to" | jq -e 'any(.tasks[]; .task == "faucet" and .points > 0)' >/dev/null
}
until_ok "the points service to credit the faucet's transfer at BTC-final" faucet_credited
curl -sSf --max-time 30 "$base/points/account/$to" | jq -e '.taking_part == false and .standing == null' >/dev/null
curl -sSf --max-time 30 "$base/points/season" | jq -e '.accounts == 0' >/dev/null
echo "points: /points follows the chain of genesis block $genesis, answers $points_origin alone, and credited the faucet task" \
    "to an account that does not take part until it links X"

# The season's official instances, deployed with the operator's key as signet/deploy.sh <host> instances does; a second
# run deploys nothing and gives the same instances
keeper=$(jq -r '.keeper' <<<"$keys")
seller=$(jq -r '.seller' <<<"$keys")
instances() {
    "${compose[@]}" run --rm --no-deps -T --user root -v "$home/config/keys/operator.key:/run/operator.key:ro" \
        -v "$home/config/points:/state" points instances --key /run/operator.key --keeper "$keeper" \
        --seller "$seller" --modules /app/wasm --state-dir /state
}
result=$(instances)
jq -e --arg g "$genesis" --arg k "$keeper" --arg s "$seller" '.genesis == $g and .keeper == $k and .seller == $s and
    ([.token, .asset, .pool, .market, .poll, .exchange] | all(test("^[0-9a-f]{64}$")))' <<<"$result" >/dev/null
[ "$(instances | jq -c .)" = "$(jq -c . <<<"$result")" ]
# The price keeper, named the market's admin, and the top-up seller, on the configuration that names the instances
jq --argjson r "$result" '.chains += $r.points | .exclude[$r.keeper] = "the price keeper" |
    .exclude[$r.seller] = "the top-up seller"' "$home/points.json" >"$home/points.json.new"
mv "$home/points.json.new" "$home/points.json"
"${compose[@]}" up -d --no-deps --force-recreate points keeper
keeper_follows() {
    local log
    log=$("${compose[@]}" logs keeper)
    grep -q "account $keeper" <<<"$log" && grep -q "the market $(jq -r '.market' <<<"$result") follows the pool" <<<"$log"
}
until_ok "the keeper to find the market following the pool" keeper_follows
until_ok "the points service again" sh -c "curl -sf --max-time 30 '$base/points/health' >/dev/null"
echo "official instances: $(jq -c '{token, pool, market, poll, exchange}' <<<"$result"); the keeper finds the market following the pool"

# The top-up seller (server/seller.ts of ksana-points): a buyer with a new key signs in (BIP322, with the SDK of the services
# image), is refused a lock until it links an X account (D-127), links one through a stand-in for X's oEmbed endpoint
# that the script runs in the btc-proxy container while it needs it (rehearsal.yml points the service at it), and asks
# again; the seller makes the lock on the official exchange, ksana-cli pays it from the rehearsal's wallet with the
# claim in the same transaction (path C), and the points service credits the top-up task once the claim's block is
# BTC-final
exchange=$(jq -r '.exchange' <<<"$result")
until_ok "the seller to offer top-ups" sh -c "curl -sf --max-time 30 '$base/points/season' | jq -e '.topup.sats == \"1000\"' >/dev/null"
asked=$("${compose[@]}" exec -T btc-proxy node --input-type=module - <<'EOF'
import { createServer } from 'node:http';
import { accountOf, encodeBip322Signature, randomBytes, signBip322Simple, toHex } from '/app/sdk/ts/dist/src/index.js';
const send = async (method, path, body, token) => {
  const res = await fetch(`http://points:8082${path}`, {
    method,
    headers: { 'content-type': 'application/json', ...(token ? { authorization: `Bearer ${token}` } : {}) },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, json: await res.json() };
};
const call = async (method, path, body, token) => {
  const { status, json } = await send(method, path, body, token);
  if (status >= 400) throw new Error(`${method} ${path}: ${status} ${JSON.stringify(json)}`);
  return json;
};
const secret = randomBytes(32);
const account = toHex(accountOf(secret));
const { message } = await call('POST', '/auth/challenge', { account });
const signature = encodeBip322Signature(signBip322Simple(secret, message));
const { token } = await call('POST', '/auth/verify', { message, signature });
const refused = await send('POST', '/topup', undefined, token);
if (refused.status !== 403 || refused.json.error?.code !== 'x_required') {
  throw new Error(`a lock for an account without X: ${refused.status} ${JSON.stringify(refused.json)}`);
}
// X's oEmbed, as it answers for a public tweet carrying the account's code
const { code } = await call('POST', '/x/code', undefined, token);
const oembed = createServer((req, res) => {
  const tweet = new URL(req.url, 'http://btc-proxy').searchParams.get('url') ?? '';
  if (!/\/status\/1$/.test(tweet)) return res.writeHead(404).end();
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end(JSON.stringify({
    url: 'https://x.com/rehearsal/status/1',
    author_name: 'rehearsal',
    author_url: 'https://x.com/rehearsal',
    html: `<blockquote class="twitter-tweet"><p lang="en" dir="ltr">Trying Ksana on its signet. ${code}</p>&mdash; rehearsal (@rehearsal)</blockquote>`,
    type: 'rich',
  }));
});
await new Promise((resolve) => oembed.listen(8099, '0.0.0.0', resolve));
const me = await call('POST', '/x/verify', { url: 'https://x.com/rehearsal/status/1' }, token).finally(() => oembed.close());
let { lock } = await call('POST', '/topup', undefined, token);
for (let i = 0; i < 90 && lock?.state === 'making'; i += 1) {
  await new Promise((resolve) => setTimeout(resolve, 2000));
  ({ lock } = await call('GET', '/topup', undefined, token));
}
console.log(JSON.stringify({ account, taking_part: me.taking_part, lock }));
EOF
)
buyer=$(jq -r '.account' <<<"$asked")
lock=$(jq -r '.lock.lock' <<<"$asked")
if ! jq -e --arg x "$exchange" '.taking_part and .lock.state == "open" and .lock.exchange == $x and .lock.ksa == "10000000000" and
    .lock.sats == "1000" and (.lock.address | startswith("tb1p"))' <<<"$asked" >/dev/null; then
    echo "the seller made no lock to pay: $asked" >&2
    exit 1
fi
# bitcoind's address and login reach ksana-cli through its environment, as the compose file gives them to the ops image
# shellcheck disable=SC2016
"${compose[@]}" run --rm --no-deps -T --entrypoint sh ops -c \
    'KSANA_BTC_RPC_URL=$BTC_RPC_URL KSANA_BTC_RPC_USER=$BTC_RPC_USER KSANA_BTC_RPC_PASS=$BTC_RPC_PASS exec ksana-cli "$@"' \
    ksana-cli --rpc http://node1:8545 top-up --exchange "$exchange" --lock "$lock" --beneficiary "$buyer" --sats 1000 \
    --btc-wallet rehearsal | jq -c '{btc_txid, ksa}'
topped_up() {
    curl -sf --max-time 30 "$base/points/account/$buyer" | jq -e 'any(.tasks[]; .task == "top_up" and .points > 0)' >/dev/null
}
until_ok "the points service to credit the top-up at BTC-final" topped_up
curl -sSf --max-time 30 "$base/points/account/$buyer" | jq -e '.taking_part and .standing.accounts == 1' >/dev/null
curl -sSf --max-time 30 "$base/points/season" | jq -e '.accounts == 1 and .counts.x_post == 1 and .counts.top_up == 1' >/dev/null
echo "top-up: $buyer was refused a lock until it linked X, then the seller locked 100 KSA for it (lock $lock)," \
    "ksana-cli paid it, and the task is credited"

# status: node1's funding from its wallet, enough of it, and no warning that nothing can settle
report=$("${compose[@]}" run --rm -T ops status)
jq -c . <<<"$report"
jq -s -e 'map(.node1 | objects | select(has("confirmed_sats"))) | length == 1 and
    (.[0] | .confirmed_sats > 0 and .outputs >= 3 and .top_up == false)' <<<"$report" >/dev/null
jq -s -e 'map(select(has("warning"))) | length == 0' <<<"$report" >/dev/null

# The relay probe, against the private signet's miner
"${compose[@]}" run --rm -T ops probe --poll-secs 3 --wait-blocks 4 | tail -8
echo "rehearsal of the hosted signet passed"
