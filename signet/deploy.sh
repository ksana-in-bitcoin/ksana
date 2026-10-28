#!/usr/bin/env bash
# Deploys the hosted public signet (D-107) from the operator's machine to a server over ssh; signet/README.md is the
# guide. The server is an Ubuntu 22.04 or 24.04 machine that `ssh <host>` reaches as root or as a user with
# passwordless sudo: the operator's own ssh keys and config do the login, and this script asks for none. What it pushes
# is the committed HEAD of this repository and of the web wallet's, ksana-web, beside it, and of the testnet points',
# ksana-points, when the server runs the points service (signet/sources.sh; KSANA_WEB_DIR and KSANA_POINTS_DIR name
# other checkouts).
#
#   signet/deploy.sh <host> check                  what the server is, what it reaches, which ports reach it
#   signet/deploy.sh <host>                        setup, push, build, keys, up: the first deploy and every later one
#                                                  (build first tags the ksana image in use ksana-prev-<commit>)
#   signet/deploy.sh <host> setup [--domain <name>] [--producers 1|2]
#                           [--former <name>|none] the signet's former domain, where the endpoints still answer
#                                                  and the page moves to the domain; none drops it
#                                                  (signet/Caddyfile, D-112)
#                           [--playground <name>|none]
#                                                  the domain of the contract playground's page (D-119), a
#                                                  Cloudflare Worker: the compile service at /compile, and the
#                                                  faucet and the compile service allow that page; none drops it
#                           [--points <name>|none] the domain of the testnet points page (D-126), a Cloudflare
#                                                  Worker: the points service at /points, which allows that page
#                                                  and sells top-ups, and the official lending market's price
#                                                  keeper; none drops them
#   signet/deploy.sh <host> push | build | btc | keys | up | down
#   signet/deploy.sh <host> web                    the web page alone: push, build its image, recreate its container
#                                                  and nothing else, then check that the live page reports the
#                                                  commit of ksana-web's HEAD
#   signet/deploy.sh <host> playground             the playground's compile service alone: push, build its image,
#                                                  recreate its container and nothing else, then check that
#                                                  /compile answers the playground's page (the first time, after
#                                                  setup --playground: the default deploy, which also gives
#                                                  Caddy /compile and the faucet the page's origin)
#   signet/deploy.sh <host> points                 the points service and the price keeper alone: push, build
#                                                  their image, recreate their containers and nothing else, then
#                                                  check that /points answers the points page (the first time,
#                                                  after setup --points: the default deploy, which also writes the
#                                                  keys of the keeper and the top-up seller and gives Caddy /points)
#   signet/deploy.sh <host> instances              the season's official instances (a token, an AMM pool, a lending
#                                                  market whose admin is the keeper, a poll, an exchange and the
#                                                  top-up seller's KSA), deployed once with the operator's key by
#                                                  the points image and written into signet/points.json of
#                                                  ksana-points and playground/hosted/deployment.json here (needs
#                                                  Node.js); commit them, then run points
#   signet/deploy.sh <host> faucet [--wait <minutes>]
#                                                  the faucet alone: push, build the services image, wait until no
#                                                  transfer waits in the faucet (at most 10 minutes unless --wait),
#                                                  recreate its container and nothing else (the BTC proxy keeps its
#                                                  own), then print the commit it runs
#   signet/deploy.sh <host> status | logs [<service>...]
#   signet/deploy.sh <host> probe [<relay probe arguments>]   does signet mine both of Ksana's carriers?
#   signet/deploy.sh <host> launch [--force]       the genesis config at the current signet tip, then every service
#                                                  (a launch cut short goes on where it stopped)
#   signet/deploy.sh <host> backup <directory>     the keys, the genesis config and .env, as a .tar.gz
#   signet/deploy.sh <host> backup-data <label> [--restart]
#                                                  stop node1, node2, the indexer and the points service (compose
#                                                  stop, nothing else), save their data volumes in
#                                                  /opt/ksana-signet/backups/<label>-<UTC time>/ with SHA256SUMS, copy
#                                                  that to ~/ksana-signet-backups here and check it; they stay
#                                                  stopped for the next deploy unless --restart
#   signet/deploy.sh <host> restore <file>         a backup onto a server that has no genesis config yet
#   signet/deploy.sh <host> reset --yes            delete the Ksana chain data and the genesis config (bitcoind and
#                                                  the keys stay): before a restart from genesis
#
# --dry-run, with the deploy, build, web, faucet, playground, points or backup-data, changes nothing: it reads the server
# and prints what would run; with instances it signs nothing and prints what the deployment would do. --help, alone or after any command but probe and logs (which pass it on), prints this and runs nothing.
# <host> is anything ssh takes (user@address, or a Host of ~/.ssh/config). Runs with the bash 3.2 of macOS.
set -euo pipefail

# usage [<exit status>]: the comment block at the top of this file; on standard error unless asked for
usage() {
    local status=${1:-2}
    if [ "$status" -eq 0 ]; then
        awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    else
        awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0" >&2
    fi
    exit "$status"
}

[ $# -ge 1 ] || usage
case "$1" in -h | --help) usage 0 ;; esac
HOST=$1
shift
DRY_RUN=no
args=()
for arg in "$@"; do
    if [ "$arg" = --dry-run ]; then
        DRY_RUN=yes
    else
        args+=("$arg")
    fi
done
# (an empty array is unbound to the bash 3.2 of macOS)
set -- ${args[@]+"${args[@]}"}
COMMAND=${1:-deploy}
[ $# -gt 0 ] && shift
# --help after a command prints the usage and runs nothing, whatever the command would make of it (down --help must
# not take the services down); probe and logs pass their arguments on, to the relay probe and to compose logs
case "$COMMAND" in
    probe | logs) ;;
    *)
        for arg in "$@"; do
            case "$arg" in -h | --help) usage 0 ;; esac
        done
        ;;
esac
case "$COMMAND" in
    -h | --help) usage 0 ;;
    deploy | build | web | faucet | playground | points | instances | backup-data) ;;
    *)
        if [ "$DRY_RUN" = yes ]; then
            echo "--dry-run works with the deploy, build, web, faucet, playground, points, instances and backup-data only" >&2
            exit 2
        fi
        ;;
esac
DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$DIR/.." && pwd)
POINTS_REPO=${KSANA_POINTS_DIR:-$REPO/../ksana-points}
SERVER_HOME=/opt/ksana-signet
# A build prints nothing for minutes at a time, and a proxy or NAT on the way may close a connection that idle; when
# the connection drops, the server cancels the build. Keepalives every 30 s hold it open. A cut build resumes where
# it stopped when the same command runs again (cargo's target directory is a cache mount).
SSH=(ssh -o ServerAliveInterval=30 -o ServerAliveCountMax=10)

# remote <arguments>: signet/server.sh on the server, as root
remote() {
    "${SSH[@]}" "$HOST" "sudo -n bash -s -- $(printf '%q ' "$@")" <"$DIR/server.sh"
}

# sources [commits]: signet/sources.sh with --points when the server runs the points service (KSANA_POINTS_ORIGIN)
sources() {
    if [ -n "$(remote points-origin)" ]; then
        "$DIR/sources.sh" "$@" --points
    else
        "$DIR/sources.sh" "$@"
    fi
}

# value <name> <lines>: the value of NAME=value among the lines
value() {
    printf '%s\n' "$2" | sed -n "s/^$1=//p"
}

# push: the sources (signet/sources.sh) into $SERVER_HOME/src, in place of what was there; the server's .env records
# their commits
push() {
    local commits
    commits=$(sources commits)
    sources tar |
        "${SSH[@]}" "$HOST" "sudo -n bash -c 'set -e; d=$SERVER_HOME; rm -rf \$d/src.new; mkdir -p \$d/src.new; tar -x -i -C \$d/src.new; rm -rf \$d/src; mv \$d/src.new \$d/src'"
    remote commit "$(value KSANA_COMMIT "$commits")" "$(value KSANA_WEB_COMMIT "$commits")" \
        "$(value KSANA_POINTS_COMMIT "$commits")"
    echo "pushed commit $(value KSANA_COMMIT "$commits") to $HOST:$SERVER_HOME/src, with ksana-web" \
        "$(value KSANA_WEB_COMMIT "$commits")"
}

# The genesis config committed in signet/genesis.json and the server's must be the same network.
check_genesis() {
    local server
    [ -f "$DIR/genesis.json" ] || return 0
    if [ "$(remote has-genesis)" != yes ]; then
        echo "note: signet/genesis.json is committed but $HOST has no genesis config: restore a backup of its keys first" \
            "(signet/deploy.sh $HOST restore <file>); until then only bitcoind runs" >&2
        return 0
    fi
    server=$(mktemp)
    remote genesis-file >"$server"
    if ! cmp -s "$server" "$DIR/genesis.json"; then
        rm -f "$server"
        echo "signet/genesis.json differs from the genesis config on $HOST: they are different networks" >&2
        exit 1
    fi
    rm -f "$server"
}

# ports <address>: how this machine reaches the server's public ports, i.e. what the firewall or security group lets
# through: "refused" means the port is open but nothing listens on it yet. Port 1, which nothing serves and no
# security group opens, tells whether the answers mean anything: a proxy in TUN mode on this machine (Clash and the
# like) answers every port itself
ports() {
    local addr=$1 port out
    if ! command -v nc >/dev/null; then
        echo "nc not found: the port check is skipped" >&2
        return 0
    fi
    if nc -z -w 5 "$addr" 1 >/dev/null 2>&1; then
        echo "ports from here: not measurable, port 1 answers too (a proxy in TUN mode on this machine answers" \
            "every port); compare the security group's inbound rules instead"
        return 0
    fi
    for port in 22 80 443 9000 9001 38333; do
        if out=$(nc -z -v -w 5 "$addr" "$port" 2>&1); then
            echo "port $port from here: open"
        elif printf '%s' "$out" | grep -qi refused; then
            echo "port $port from here: reachable, nothing listening yet"
        else
            echo "port $port from here: no answer (closed by a firewall or security group?)"
        fi
    done
}

deploy() {
    if [ "$DRY_RUN" = yes ]; then
        echo "would run setup, and push commit $(git -C "$REPO" rev-parse --short=9 HEAD) to $HOST:$SERVER_HOME/src"
        sources commits >/dev/null
        check_genesis
        remote --dry-run build
        echo "would then run keys and up"
        return 0
    fi
    remote setup - -
    push
    check_genesis
    remote build
    remote keys
    remote up
    if [ "$(remote has-genesis)" != yes ]; then
        cat <<EOF

bitcoind is syncing Bitcoin's public signet (a few hours the first time). Next (signet/README.md):
  1. send signet BTC to node1's funding address and to the probe address (printed above);
  2. once bitcoind has synced:  signet/deploy.sh $HOST probe
  3. when the probe's transactions are all mined:  signet/deploy.sh $HOST launch
  4. commit signet/genesis.json, which launch writes.
EOF
    fi
}

# Starts every service of a server that has its genesis config, and copies the config to signet/genesis.json.
start_network() {
    remote up
    remote genesis-file >"$DIR/genesis.json.tmp"
    mv "$DIR/genesis.json.tmp" "$DIR/genesis.json"
    echo "the signet is running; signet/genesis.json is its genesis config: commit it so that others can join"
}

launch() {
    local sats
    if [ "$(remote has-genesis)" = yes ]; then
        # A launch cut short after the genesis config was written goes on from there; one with another network's
        # config here does not
        if [ ! -f "$DIR/genesis.json" ] || remote genesis-file | cmp -s - "$DIR/genesis.json"; then
            echo "$HOST already has its genesis config: starting the services and copying the config back" >&2
            start_network
            return 0
        fi
        echo "$HOST runs another network than signet/genesis.json (reset --yes starts it over)" >&2
        exit 1
    fi
    remote keys >/dev/null
    sats=$(remote funded)
    if [ "${sats:-0}" -le 0 ] && [ "${1:-}" != --force ]; then
        echo "node1's funding address holds no confirmed signet BTC: fund it first (signet/deploy.sh $HOST keys prints" \
            "it), or pass --force" >&2
        exit 1
    fi
    remote genesis
    # the genesis timestamp is ASERT's anchor (D-026): start right away
    start_network
}

backup() {
    local dir=${1:?usage: signet/deploy.sh <host> backup <directory>} file
    mkdir -p "$dir"
    file="$dir/ksana-signet-$(date +%Y%m%d-%H%M%S).tar.gz"
    (
        umask 077
        remote backup >"$file"
    )
    echo "saved $file: it holds the private keys of the faucet, the seed nodes and the block share account; keep it private"
}

# live_commit <domain>: the commit the live page reports (its footer and Settings → About show it): ksana-web's
# vite.config.ts writes it into the page's main script, next to that repository's URL. Fails when the page or the commit
# is not found
live_commit() {
    local base="https://$1" script js q="[\`'\"]"
    script=$(curl -fsS --max-time 20 "$base/" | grep -o 'assets/index-[A-Za-z0-9_-]*\.js' | sed -n 1p) || return 1
    [ -n "$script" ] || return 1
    js=$(curl -fsS --max-time 60 "$base/$script") || return 1
    printf '%s' "$js" | grep -oE "ksana-in-bitcoin/ksana-web${q},[A-Za-z_\$][A-Za-z0-9_\$]*=${q}[0-9a-f]*${q}" |
        sed -n '1s/.*=.\([0-9a-f]*\).$/\1/p'
}

# web: the web page alone (ksana-web and the SDK it pins, signet/Caddyfile, signet/deployment.json). The sources are
# pushed, the web image built and its container recreated; then the live page must report the commit of ksana-web's
# HEAD (Caddy is given a minute to start)
web() {
    local commit domain live='' tries=0
    commit=$(value KSANA_WEB_COMMIT "$(sources commits)")
    if [ "$DRY_RUN" = yes ]; then
        echo "would push the sources to $HOST:$SERVER_HOME/src"
        remote --dry-run web
    else
        push
        remote web
    fi
    domain=$(remote domain)
    if [ -z "$domain" ]; then
        echo "$HOST has no KSANA_SIGNET_DOMAIN in its .env: the page's commit is not checked" >&2
        exit 1
    fi
    while :; do
        live=$(live_commit "$domain") || live=
        if [ "$DRY_RUN" = yes ] || [ "$live" = "$commit" ] || [ "$tries" -ge 12 ]; then
            break
        fi
        tries=$((tries + 1))
        sleep 5
    done
    if [ -z "$live" ]; then
        echo "https://$domain/ reports no commit (unreachable from here, or no commit in its main script):" \
            "check the page's footer" >&2
        exit 1
    fi
    echo "https://$domain/ reports build $live; ksana-web's HEAD is $commit"
    if [ "$DRY_RUN" = no ] && [ "$live" != "$commit" ]; then
        echo "the live page is not the build of ksana-web's HEAD" >&2
        exit 1
    fi
}

# faucet [--wait <minutes>]: the KSA faucet alone (signet/services/faucet.mjs, the SDK it runs on). The committed HEAD is
# pushed, the services image built, and once no transfer waits in the faucet its container alone is recreated
# (server.sh faucet); the BTC proxy, whose image it shares, keeps its container. The faucet must then run HEAD's build
# and answer at https://<domain>/faucet
faucet() {
    local minutes=10 commit running domain body waiting='' tries=0
    while [ $# -gt 0 ]; do
        case "$1" in
            --wait) minutes=${2:?--wait needs a number of minutes} && shift 2 ;;
            *) usage ;;
        esac
    done
    case "$minutes" in
        0* | *[!0-9]*)
            echo "usage: signet/deploy.sh <host> faucet [--wait <minutes>]; the wait is a whole number of minutes" >&2
            exit 2
            ;;
    esac
    commit=$(git -C "$REPO" rev-parse --short=9 HEAD)
    if [ "$DRY_RUN" = yes ]; then
        echo "would push the sources (commit $commit) to $HOST:$SERVER_HOME/src"
        remote --dry-run faucet "$minutes"
    else
        push
        if ! running=$(remote faucet "$minutes" | tail -n 1); then
            echo "the faucet's update stopped on $HOST: the messages above say whether its container was recreated" >&2
            exit 1
        fi
        echo "the faucet runs commit $running; HEAD is $commit"
        if [ "$running" != "$commit" ]; then
            echo "the faucet does not run HEAD's build" >&2
            exit 1
        fi
    fi
    domain=$(remote domain)
    if [ -z "$domain" ]; then
        echo "$HOST has no KSANA_SIGNET_DOMAIN in its .env: https://<domain>/faucet is not checked" >&2
        exit 1
    fi
    while :; do
        body=$(curl -fsS --max-time 20 "https://$domain/faucet") || body=
        waiting=$(printf '%s' "$body" | sed -n 's/.*"waiting":\([0-9][0-9]*\).*/\1/p')
        if [ -n "$waiting" ] || [ "$DRY_RUN" = yes ] || [ "$tries" -ge 12 ]; then
            break
        fi
        tries=$((tries + 1))
        sleep 5
    done
    if [ -z "$waiting" ]; then
        echo "https://$domain/faucet does not answer from here" >&2
        exit 1
    fi
    echo "https://$domain/faucet answers: $waiting transfer(s) waiting"
}

# playground: the contract playground's compile service alone (playground/compiler, the example contracts and the
# workspace it builds them in; its page is a Cloudflare Worker, playground/wrangler.jsonc). The committed HEAD is pushed,
# the compiler's image built and its container recreated (server.sh playground); then https://<domain>/compile/health
# must answer (the container is given a minute to start) and a preflight from the page's origin must be allowed. It ends
# with the build the page reports, which Workers Builds deploys from main
playground() {
    local commit domain origin health='' allowed='' live='' tries=0
    commit=$(git -C "$REPO" rev-parse --short=9 HEAD)
    origin=$(remote playground-origin)
    if [ -z "$origin" ]; then
        echo "$HOST has no KSANA_PLAYGROUND_ORIGIN in its .env: run signet/deploy.sh $HOST setup --playground <name> first" >&2
        exit 1
    fi
    domain=$(remote domain)
    if [ "$DRY_RUN" = yes ]; then
        echo "would push the sources (commit $commit) to $HOST:$SERVER_HOME/src"
        remote --dry-run playground
    else
        push
        remote playground
    fi
    while :; do
        health=$(curl -fsS --max-time 20 "https://$domain/compile/health") || health=
        if [ "$DRY_RUN" = yes ] || [ -n "$health" ] || [ "$tries" -ge 12 ]; then
            break
        fi
        tries=$((tries + 1))
        sleep 5
    done
    if [ -z "$health" ]; then
        echo "https://$domain/compile/health does not answer: signet/deploy.sh $HOST logs compiler shows why" >&2
        [ "$DRY_RUN" = yes ] || exit 1
    else
        echo "https://$domain/compile answers: $(printf '%s' "$health" | sed -n 's/.*"rustc":"\([^"]*\)".*/\1/p')"
    fi
    # The page sends its sources as JSON from its own origin: the browser asks first
    allowed=$(curl -sS -o /dev/null -D - --max-time 20 -X OPTIONS -H "Origin: $origin" \
        -H 'Access-Control-Request-Method: POST' -H 'Access-Control-Request-Headers: content-type' \
        "https://$domain/compile" | tr -d '\r' | grep -i '^access-control-allow-origin:' | sed 's/^[^:]*: *//') || allowed=
    if [ "$allowed" != "$origin" ]; then
        echo "https://$domain/compile does not allow the page at $origin (it allows ${allowed:-no origin})" >&2
        [ "$DRY_RUN" = yes ] || exit 1
    else
        echo "https://$domain/compile allows the page at $origin"
    fi
    live=$(curl -fs --max-time 20 "$origin/version.json" | sed -n 's/.*"commit":"\([0-9a-f]*\)".*/\1/p') || live=
    echo "$origin/ reports build ${live:-none} (Workers Builds deploys it from main); HEAD is $commit"
}

# points: the points service and the price keeper alone (ksana-points, D-126: the service, its configuration
# signet/points.json, the SDK and the example contracts of the core commit it pins; its page is a Cloudflare Worker,
# wrangler.jsonc of ksana-points). The sources are pushed, the points image built and the two containers recreated
# (server.sh points); then https://<domain>/points/health must answer (the service is given a minute to start) and a
# preflight from the page's origin must be allowed
points() {
    local commit domain origin health='' allowed='' tries=0
    commit=$(git -C "$REPO" rev-parse --short=9 HEAD)
    origin=$(remote points-origin)
    if [ -z "$origin" ]; then
        echo "$HOST has no KSANA_POINTS_ORIGIN in its .env: run signet/deploy.sh $HOST setup --points <name> first" >&2
        exit 1
    fi
    domain=$(remote domain)
    if [ "$DRY_RUN" = yes ]; then
        echo "would push the sources (commit $commit) to $HOST:$SERVER_HOME/src"
        remote --dry-run points
    else
        push
        remote points
    fi
    while :; do
        health=$(curl -fsS --max-time 20 "https://$domain/points/health") || health=
        if [ "$DRY_RUN" = yes ] || [ -n "$health" ] || [ "$tries" -ge 12 ]; then
            break
        fi
        tries=$((tries + 1))
        sleep 5
    done
    if [ -z "$health" ]; then
        echo "https://$domain/points/health does not answer: signet/deploy.sh $HOST logs points shows why" >&2
        [ "$DRY_RUN" = yes ] || exit 1
    else
        echo "https://$domain/points answers: $health"
    fi
    # The page calls it with JSON bodies and a bearer token from its own origin: the browser asks first
    allowed=$(curl -sS -o /dev/null -D - --max-time 20 -X OPTIONS -H "Origin: $origin" \
        -H 'Access-Control-Request-Method: POST' -H 'Access-Control-Request-Headers: content-type, authorization' \
        "https://$domain/points/auth/challenge" | tr -d '\r' | grep -i '^access-control-allow-origin:' |
        sed 's/^[^:]*: *//') || allowed=
    if [ "$allowed" != "$origin" ]; then
        echo "https://$domain/points does not allow the page at $origin (it allows ${allowed:-no origin})" >&2
        [ "$DRY_RUN" = yes ] || exit 1
    else
        echo "https://$domain/points allows the page at $origin"
    fi
}

# instances: the season's official instances (server/official.ts of ksana-points, D-126), deployed on the server by the
# points image with the operator's key (server.sh instances), then written into signet/points.json of ksana-points,
# which names them to the points service, and playground/hosted/deployment.json here, which offers them on the
# playground's pages (scripts/apply-instances.mjs of ksana-points, Node.js). A run cut short goes on where it stopped;
# one after every step is done deploys nothing and gives the same instances. Commit the two files, then run points
instances() {
    local result
    if [ "$DRY_RUN" = yes ]; then
        remote --dry-run instances
        return 0
    fi
    if ! command -v node >/dev/null; then
        echo "instances writes the instances into signet/points.json of ksana-points with Node.js, which is not" \
            "found here" >&2
        exit 1
    fi
    if [ ! -f "$POINTS_REPO/scripts/apply-instances.mjs" ]; then
        echo "ksana-points not found at $POINTS_REPO: clone it there or name its checkout with KSANA_POINTS_DIR" >&2
        exit 1
    fi
    result=$(mktemp)
    if ! remote instances >"$result" || [ ! -s "$result" ]; then
        rm -f "$result"
        echo "the deployment of the official instances stopped on $HOST: the messages above say where; running" \
            "instances again goes on from there" >&2
        exit 1
    fi
    node "$POINTS_REPO/scripts/apply-instances.mjs" "$result" "$REPO/playground/hosted/deployment.json"
    rm -f "$result"
    echo "commit signet/points.json in ksana-points and playground/hosted/deployment.json here, then run" \
        "signet/deploy.sh $HOST points"
}

# sha256_check <directory>: the files in the directory against its SHA256SUMS
sha256_check() {
    if command -v sha256sum >/dev/null; then
        (cd "$1" && sha256sum -c SHA256SUMS)
    else
        (cd "$1" && shasum -a 256 -c SHA256SUMS)
    fi
}

# backup-data <label> [--restart]: the data volumes of the seed nodes and the indexer, saved and checked on the server
# (server.sh backup-data), then copied to ~/ksana-signet-backups/<label>-<UTC time> here and checked again
backup_data() {
    local label='' restart=no name dest="$HOME/ksana-signet-backups"
    while [ $# -gt 0 ]; do
        case "$1" in
            --restart) restart=yes ;;
            -*) usage ;;
            *)
                [ -z "$label" ] || usage
                label=$1
                ;;
        esac
        shift
    done
    case "$label" in
        "" | [!A-Za-z0-9]* | *[!A-Za-z0-9._-]*)
            echo "usage: signet/deploy.sh <host> backup-data <label> [--restart]; a label is letters, digits," \
                "'.', '_' and '-', and starts with a letter or a digit" >&2
            exit 2
            ;;
    esac
    if [ "$DRY_RUN" = yes ]; then
        name=$(remote --dry-run backup-data "$label" "$restart" | tail -n 1)
        echo "would copy $HOST:$SERVER_HOME/backups/$name to $dest/$name and check it against its SHA256SUMS"
        return 0
    fi
    if ! name=$(remote backup-data "$label" "$restart" | tail -n 1); then
        echo "the backup failed on $HOST; node1, node2 and the indexer may be stopped: signet/deploy.sh $HOST status" \
            "shows them, signet/deploy.sh $HOST up starts them" >&2
        exit 1
    fi
    case "$name" in
        "$label"-*) ;;
        *)
            echo "the server named no backup directory (got '$name')" >&2
            exit 1
            ;;
    esac
    mkdir -p "$dest"
    "${SSH[@]}" "$HOST" "sudo -n tar -C $SERVER_HOME/backups -cf - $(printf %q "$name")" |
        (umask 077 && tar -C "$dest" -xf -)
    sha256_check "$dest/$name"
    echo "saved $dest/$name (and $HOST:$SERVER_HOME/backups/$name): the seed nodes' volumes hold their P2P keys" \
        "(p2p.key), so keep it private"
}

restore() {
    local file=${1:?usage: signet/deploy.sh <host> restore <backup .tar.gz>}
    if [ "$(remote has-genesis)" = yes ]; then
        echo "$HOST already has a genesis config; restore only onto a server without one" >&2
        exit 1
    fi
    "${SSH[@]}" "$HOST" "sudo -n bash -c 'set -e; mkdir -p $SERVER_HOME; tar -xz -C $SERVER_HOME; chmod 700 $SERVER_HOME'" <"$file"
    echo "restored $file onto $HOST; signet/deploy.sh $HOST starts it"
}

case "$COMMAND" in
    deploy) deploy ;;
    check)
        remote check
        ports "$(ssh -G "$HOST" | awk '$1 == "hostname" { print $2; exit }')"
        ;;
    setup)
        domain=- producers=- former=- playground=- points=-
        while [ $# -gt 0 ]; do
            case "$1" in
                --domain) domain=${2:?--domain needs a name} && shift 2 ;;
                --producers) producers=${2:?--producers needs 1 or 2} && shift 2 ;;
                --former) former=${2:?--former needs a name, or none} && shift 2 ;;
                --playground) playground=${2:?--playground needs a name, or none} && shift 2 ;;
                --points) points=${2:?--points needs a name, or none} && shift 2 ;;
                *) usage ;;
            esac
        done
        case "$producers" in -| 1 | 2) ;; *) usage ;; esac
        remote setup "$domain" "$producers" "$former" "$playground" "$points"
        ;;
    push) push ;;
    build)
        if [ "$DRY_RUN" = yes ]; then
            remote --dry-run build
        else
            remote build
        fi
        ;;
    btc | keys | down) remote "$COMMAND" ;;
    web) web ;;
    playground) playground ;;
    points) points ;;
    instances) instances ;;
    faucet) faucet "$@" ;;
    up)
        check_genesis
        remote up
        ;;
    status | logs | probe) remote "$COMMAND" "$@" ;;
    launch) launch "$@" ;;
    backup) backup "$@" ;;
    backup-data) backup_data "$@" ;;
    restore) restore "$@" ;;
    reset)
        if [ "${1:-}" != --yes ]; then
            echo "reset deletes the Ksana chain data and the genesis config on $HOST (bitcoind and the keys stay); pass --yes" >&2
            exit 1
        fi
        remote reset
        ;;
    *) usage ;;
esac
