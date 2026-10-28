#!/bin/bash
# The server side of signet/deploy.sh (D-107): deploy.sh sends this script over ssh and runs it as root on the
# server (Ubuntu 22.04 or 24.04), one command per call. Everything lives in $KSANA_SIGNET_HOME (default
# /opt/ksana-signet):
#
#   .env            the compose settings (signet/.env.example): bitcoind's RPC password, the domains, the producers
#   config/         the keys and genesis.json (signet/ksana-signet.sh), mounted into the containers at /signet
#   src/            the sources at the deployed commits (deploy.sh push, signet/sources.sh): this repository, with
#                   ksana-web at web/ and ksana-points at points/; compose builds the images from it
#   ksana-builds    the ksana and services images built here, each with the commit it was built from (build, faucet)
#   backups/        the data volumes of the seed nodes, the indexer and the points service, a directory per backup
#                   (backup-data)
#
# With --dry-run before the command, build, web, faucet, playground, points and backup-data change nothing: they print
# what they would run; instances signs nothing and prints what it would do.
set -euo pipefail

HOME_DIR=${KSANA_SIGNET_HOME:-/opt/ksana-signet}
ENV_FILE=$HOME_DIR/.env
SRC=$HOME_DIR/src
GENESIS=$HOME_DIR/config/genesis.json
BUILDS=$HOME_DIR/ksana-builds
DRY_RUN=no

log() {
    echo "[server] $*" >&2
}

env_value() {
    sed -n "s/^$1=//p" "$ENV_FILE" 2>/dev/null | tail -1 | tr -d "'\""
}

# COMPOSE: the docker compose command of the deployed tree, with the node2 profile when there are two producers, the
# playground profile (its compile service) when the playground's page has an origin (D-119), and the points profile
# (the points service and the price keeper) when the points page has one (D-126)
compose_command() {
    COMPOSE=(docker compose --project-directory "$SRC/signet" -f "$SRC/signet/docker-compose.yml"
        --env-file "$ENV_FILE")
    if [ "$(env_value KSANA_SIGNET_PRODUCERS)" = 2 ]; then
        COMPOSE+=(--profile node2)
    fi
    if [ -n "$(env_value KSANA_PLAYGROUND_ORIGIN)" ]; then
        COMPOSE+=(--profile playground)
    fi
    if [ -n "$(env_value KSANA_POINTS_ORIGIN)" ]; then
        COMPOSE+=(--profile points)
    fi
}

compose() {
    compose_command
    "${COMPOSE[@]}" "$@"
}

# run <command...>: a command that changes something; with --dry-run it is only printed
run() {
    if [ "$DRY_RUN" = no ]; then
        "$@"
        return
    fi
    if [ "$1" = compose ]; then
        shift
        compose_command
        set -- "${COMPOSE[@]}" "$@"
    fi
    log "would run: $*"
}

# container <service>: the ID of the service's container in this project, running or stopped; empty without one
container() {
    docker ps -a -q --filter label=com.docker.compose.project=ksana-signet \
        --filter "label=com.docker.compose.service=$1" --filter label=com.docker.compose.oneoff=False | sed -n 1p
}

require_src() {
    if [ ! -f "$SRC/signet/docker-compose.yml" ]; then
        log "nothing deployed yet: run signet/deploy.sh <host> push"
        exit 1
    fi
}

# require_points: the points profile is on (KSANA_POINTS_ORIGIN) and the keys of the keeper and the top-up seller
# exist. Their containers mount those single files, and Docker would make a directory in place of a missing one
require_points() {
    local k
    if [ -z "$(env_value KSANA_POINTS_ORIGIN)" ]; then
        log "no KSANA_POINTS_ORIGIN in $ENV_FILE: run signet/deploy.sh <host> setup --points <name> first"
        exit 1
    fi
    for k in keeper seller; do
        if [ ! -f "$HOME_DIR/config/keys/$k.key" ]; then
            log "no $k key in $HOME_DIR/config/keys: the default deploy (signet/deploy.sh <host>) writes it with the" \
                "other keys"
            exit 1
        fi
    done
}

install_docker() {
    if docker compose version >/dev/null 2>&1 && docker buildx version >/dev/null 2>&1; then
        return
    fi
    log "installing Docker Engine from Docker's apt repository"
    apt-get update -q
    apt-get install -y -q ca-certificates curl gnupg jq openssl
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
    chmod a+r /etc/apt/keyrings/docker.asc
    # shellcheck disable=SC1091
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
        >/etc/apt/sources.list.d/docker.list
    apt-get update -q
    apt-get install -y -q docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
    systemctl enable --now docker
}

# 4 GiB of swap on a machine with less than 8 GiB of memory: building the Rust binaries in release mode next to a
# syncing bitcoind needs it.
add_swap() {
    local mem_kib
    mem_kib=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
    if [ -n "$(swapon --show --noheadings)" ] || [ "$mem_kib" -ge 8000000 ]; then
        return
    fi
    log "adding a 4 GiB swap file"
    fallocate -l 4G /swapfile
    chmod 600 /swapfile
    mkswap /swapfile >/dev/null
    swapon /swapfile
    grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >>/etc/fstab
}

# setup <domain or -> <producers or -> [<former domain or ->] [<playground domain or ->] [<points domain or ->]: Docker,
# swap, the directory and .env. An existing .env keeps its password and its values, except the ones given again (- keeps
# one; none takes the former domain, the playground or the points page away). The signet's names must resolve to this
# server alone. At the signet's former domain the endpoints stay and the page moves to the domain (signet/Caddyfile,
# D-112). The playground's page (D-119) is a Cloudflare Worker at https://<playground domain>, which this server lets call
# its faucet and compile service; the points page (D-126) is one at https://<points domain>, which it lets call /points.
setup() {
    local domain=${1:--} producers=${2:--} former=${3:--} playground=${4:--} points=${5:--} given=yes former_given=yes
    local origin points_origin ip pass salt hmac msg
    install_docker
    add_swap
    apt-get install -y -q jq openssl >/dev/null 2>&1 || true
    if [ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo unknown)" != yes ]; then
        log "WARNING: the clock is not synchronized (timedatectl); Ksana rejects blocks more than 15 s in the future"
    fi
    mkdir -p "$HOME_DIR/config"
    chmod 700 "$HOME_DIR"
    if [ ! -f "$ENV_FILE" ]; then
        pass=$(openssl rand -hex 24)
        salt=$(openssl rand -hex 16)
        hmac=$(printf '%s' "$pass" | openssl dgst -sha256 -hmac "$salt" | awk '{print $NF}')
        {
            echo "# Written by signet/deploy.sh setup; see signet/.env.example"
            echo "KSANA_SIGNET_HOME=$HOME_DIR"
            echo "BTC_RPC_PASS=$pass"
            # single quotes: compose would read \$ as the start of a variable
            echo "BTC_RPCAUTH='ksana:$salt\$$hmac'"
            echo "KSANA_SIGNET_PRODUCERS=1"
            echo "KSANA_FAUCET_KSA=100"
            echo "KSANA_FAUCET_COOLDOWN_HOURS=24"
            echo "KSANA_COMMIT="
        } >"$ENV_FILE"
        chmod 600 "$ENV_FILE"
    fi
    [ "$producers" != - ] || producers=$(env_value KSANA_SIGNET_PRODUCERS)
    if [ "$domain" = - ]; then
        domain=$(env_value KSANA_SIGNET_DOMAIN)
        given=no
    fi
    case "$former" in
        -)
            former=$(env_value KSANA_SIGNET_FORMER_DOMAIN)
            former_given=no
            ;;
        none) former='' ;;
    esac
    case "$playground" in
        -) origin=$(env_value KSANA_PLAYGROUND_ORIGIN) ;;
        none) origin='' ;;
        *)
            # A name, which Cloudflare serves (playground/wrangler.jsonc): it resolves elsewhere, and is not checked
            case "$playground" in
                *[!A-Za-z0-9.-]* | .* | *. | -* | *-)
                    log "ERROR: --playground takes the page's domain name, such as playground.ksana.network, not $playground"
                    exit 1
                    ;;
            esac
            origin=https://$playground
            ;;
    esac
    case "$points" in
        -) points_origin=$(env_value KSANA_POINTS_ORIGIN) ;;
        none) points_origin='' ;;
        *)
            # A name, which Cloudflare serves (wrangler.jsonc of ksana-points): it resolves elsewhere, and is not checked
            case "$points" in
                *[!A-Za-z0-9.-]* | .* | *. | -* | *-)
                    log "ERROR: --points takes the page's domain name, such as points.ksana.network, not $points"
                    exit 1
                    ;;
            esac
            points_origin=https://$points
            ;;
    esac
    ip=$(curl -fsS4 --max-time 10 https://api.ipify.org || true)
    if [ -z "$domain" ]; then
        # No domain of our own: sslip.io resolves <ip with dashes>.sslip.io to the address itself
        [ -n "$ip" ] || ip=$(hostname -I | awk '{print $1}')
        domain="${ip//./-}.sslip.io"
    fi
    # Checked before anything is saved, so that a refused name never reaches .env. The domain as its own former name
    # would send the page to itself
    if [ "$former" = "$domain" ]; then
        log "ERROR: $domain is given as both the signet's domain and its former domain"
        exit 1
    fi
    if [ -n "$origin" ] && { [ "$origin" = "https://$domain" ] || [ "$origin" = "https://$former" ]; }; then
        log "ERROR: the playground's page needs a domain of its own, not ${origin#https://}"
        exit 1
    fi
    if [ -n "$points_origin" ] && { [ "$points_origin" = "https://$domain" ] || [ "$points_origin" = "https://$former" ] ||
        [ "$points_origin" = "$origin" ]; }; then
        log "ERROR: the points page needs a domain of its own, not ${points_origin#https://}"
        exit 1
    fi
    if [ -n "$ip" ]; then
        [ "$domain" = "${ip//./-}.sslip.io" ] || check_name "$domain" "$given" "$ip"
        [ -z "$former" ] || check_name "$former" "$former_given" "$ip"
    fi
    set_env KSANA_SIGNET_PRODUCERS "$producers"
    # The faucet sends through node2 when there is one, so that a client holding node1's connections (the page's
    # /rpc) does not cut it off
    if [ "$producers" = 2 ]; then
        set_env KSANA_FAUCET_NODE node2
    else
        set_env KSANA_FAUCET_NODE node1
    fi
    set_env KSANA_SIGNET_DOMAIN "$domain"
    set_env KSANA_SIGNET_FORMER_DOMAIN "$former"
    set_env KSANA_PLAYGROUND_ORIGIN "$origin"
    set_env KSANA_POINTS_ORIGIN "$points_origin"
    msg="domain $domain${former:+ (formerly $former)}, $producers producing seed node(s)"
    msg+="${origin:+, the playground at $origin}${points_origin:+, the points page at $points_origin}"
    log "$msg; settings in $ENV_FILE"
}

# check_name <name> <given: yes or no> <this server's address>: Caddy's certificate (Let's Encrypt over ports 80 and
# 443) and the seed node's P2P address need the name to point here alone, with no proxy in front (on Cloudflare, "DNS
# only"). A name given to setup is refused otherwise; one kept from .env (a later deploy) is only warned about.
check_name() {
    local name=$1 given=$2 ip=$3 resolved msg
    resolved=$({ getent ahostsv4 "$name" || true; } | awk '{print $1}' | sort -u | paste -sd ' ' -)
    if [ "$resolved" != "$ip" ]; then
        msg="$name resolves to ${resolved:-nothing}, not to this server's address $ip: point its A record here (DNS only) and wait out the record's TTL"
        if [ "$given" = yes ]; then
            log "ERROR: $msg, then run setup again"
            exit 1
        fi
        log "WARNING: $msg"
    fi
    # The compose network has no IPv6: visitors over IPv6 would reach the faucet and the BTC proxy from one gateway
    # address and share every per-address limit
    resolved=$({ getent ahostsv6 "$name" || true; } | awk '$1 !~ /^::ffff:/ { print $1 }' | sort -u | paste -sd ' ' -)
    if [ -n "$resolved" ]; then
        log "WARNING: $name has an AAAA record ($resolved): remove it, or IPv6 visitors share one address in the faucet's and the BTC proxy's limits"
    fi
}

set_env() {
    if grep -q "^$1=" "$ENV_FILE"; then
        sed -i "s|^$1=.*|$1=$2|" "$ENV_FILE"
    else
        echo "$1=$2" >>"$ENV_FILE"
    fi
}

# check: what the server is and what it reaches, before the first deploy; reads only, needs nothing installed
check() {
    local md=http://metadata.tencentyun.com/latest/meta-data region id url out peer a b total i
    # shellcheck disable=SC1091
    echo "os: $(. /etc/os-release && echo "$PRETTY_NAME"), kernel $(uname -r), $(uname -m)"
    echo "cpu: $(nproc) cores, $(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -1)"
    echo "memory: $(free -m | awk '/^Mem:/ {print $2}') MiB, swap $(free -m | awk '/^Swap:/ {print $2}') MiB"
    echo "disk /: $(df -h --output=size,avail / | tail -1 | awk '{print $1 " in all, " $2 " free"}')"
    # Time the hypervisor took from this machine over 5 seconds: a shared or burstable CPU shows it under load
    read -r -a a </proc/stat
    sleep 5
    read -r -a b </proc/stat
    total=0
    for i in 1 2 3 4 5 6 7 8; do total=$((total + b[i] - a[i])); done
    echo "cpu steal: $(((b[8] - a[8]) * 100 / (total > 0 ? total : 1)))% over 5 s"
    echo "clock synchronized: $(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo unknown)"
    if curl -sf --max-time 3 "$md/" >/dev/null 2>&1; then
        region=$(curl -sf --max-time 3 "$md/placement/region" || true)
        id=$(curl -sf --max-time 3 "$md/instance-id" || true)
        echo "tencent cloud: region $region, zone $(curl -sf --max-time 3 "$md/placement/zone" || true), instance $id," \
            "type $(curl -sf --max-time 3 "$md/instance/instance-type" || true)"
        case "$region" in
            ap-hongkong | ap-taipei | ap-singapore | ap-tokyo | ap-seoul | ap-bangkok | ap-jakarta | ap-mumbai | na-* | eu-* | sa-*) ;;
            *) echo "WARNING: $region is in mainland China: a domain on ports 80 and 443 needs an ICP filing there" ;;
        esac
        # The metadata names the CVM even under a Lighthouse instance (lhins- in the console)
        echo "note: on a Lighthouse instance the outbound traffic counts against its monthly package; bitcoind uploads" \
            "at most about 1 GB a day (-maxuploadtarget), and signet/deploy.sh <host> status shows the totals"
    fi
    echo "public address: $(curl -fsS4 --max-time 10 https://api.ipify.org 2>/dev/null || echo unknown)"
    # What the first deploy downloads from (Docker, the images, crates, Debian and npm packages, the certificate)
    for url in https://download.docker.com/linux/ubuntu/gpg https://registry-1.docker.io/v2/ https://index.crates.io/config.json \
        https://static.crates.io/crates/serde/serde-1.0.200.crate https://github.com/ https://deb.debian.org/debian/dists/bookworm/Release \
        https://registry.npmjs.org/borsh https://acme-v02.api.letsencrypt.org/directory 'https://speed.cloudflare.com/__down?bytes=25000000'; do
        out=$(curl -s -o /dev/null --max-time 60 -w '%{http_code} in %{time_total} s, %{size_download} bytes at %{speed_download} B/s' "$url" 2>/dev/null) || true
        echo "$url: ${out:-no answer}"
    done
    for peer in inquisition.bitcoin-signet.net seed.signet.bitcoin.sprovoost.nl; do
        if timeout 10 bash -c "exec 3<>/dev/tcp/$peer/38333" 2>/dev/null; then
            echo "signet peer $peer:38333 reachable"
        else
            echo "signet peer $peer:38333 unreachable"
        fi
    done
}

# traffic: the bytes the public interface received and sent since the server booted (a Lighthouse instance's monthly
# package counts what it sends)
traffic() {
    local dev
    dev=$(awk '$2 == "00000000" { print $1; exit }' /proc/net/route)
    [ -n "$dev" ] || return 0
    echo "{\"traffic\": {\"interface\": \"$dev\", \"hours_since_boot\": $(awk '{ print int($1 / 3600) }' /proc/uptime)," \
        "\"received_mb\": $(($(cat "/sys/class/net/$dev/statistics/rx_bytes") / 1000000))," \
        "\"sent_mb\": $(($(cat "/sys/class/net/$dev/statistics/tx_bytes") / 1000000))}}"
}

# The ksana services start only once the genesis config exists (launch); bitcoind always.
up() {
    require_src
    if [ -f "$GENESIS" ]; then
        [ -z "$(env_value KSANA_POINTS_ORIGIN)" ] || require_points
        compose up -d --remove-orphans
    else
        compose up -d bitcoind
        log "no genesis config yet: only bitcoind runs (signet/README.md: probe, then launch)"
    fi
}

# tag_prev, before a build: the ksana image in use (that of the seed nodes' and the indexer's containers, even stopped,
# else the one tagged ksana-signet:ksana) is tagged ksana-signet:ksana-prev-<the commit it was built from>, or
# ksana-prev-<its image ID> when it was built before the builds were recorded. The build moves ksana-signet:ksana and
# prunes the images left without a tag; this one stays for a rollback (signet/README.md)
tag_prev() {
    local service id image='' commit='' user=''
    for service in node1 node2 indexer; do
        id=$(container "$service")
        if [ -n "$id" ]; then
            image=$(docker inspect -f '{{.Image}}' "$id")
            user=$service
            break
        fi
    done
    if [ -z "$image" ]; then
        image=$(docker image inspect -f '{{.Id}}' ksana-signet:ksana 2>/dev/null || true)
        user="no container, tagged ksana-signet:ksana"
    fi
    if [ -z "$image" ]; then
        log "no ksana image yet: nothing to keep"
        return 0
    fi
    commit=$(built_from "$image")
    if [ -n "$commit" ]; then
        log "the ksana image in use ($user) is ${image#sha256:}, built from commit $commit"
    else
        commit=${image#sha256:}
        commit=${commit:0:12}
        log "the ksana image in use ($user) is ${image#sha256:}, built before the builds were recorded"
    fi
    log "keeping it as ksana-signet:ksana-prev-$commit"
    run docker tag "$image" "ksana-signet:ksana-prev-$commit"
}

# record_build <image>..., after a build: each image just built and the commit of src/ (KSANA_COMMIT, deploy.sh push),
# ksana-signet:ksana for tag_prev and ksana-signet:services for the commit the faucet runs; the latest 50 records
record_build() {
    local commit name image
    commit=$(env_value KSANA_COMMIT)
    [ -n "$commit" ] || return 0
    for name in "$@"; do
        image=$(docker image inspect -f '{{.Id}}' "$name")
        {
            if [ -f "$BUILDS" ]; then
                grep -v "^$image " "$BUILDS" || true
            fi
            echo "$image $commit"
        } | tail -n 50 >"$BUILDS.tmp"
        mv "$BUILDS.tmp" "$BUILDS"
    done
}

# built_from <image ID>: the commit the image was built from, as the build record has it; empty when it is not there
built_from() {
    if [ -f "$BUILDS" ]; then
        awk -v id="$1" '$1 == id { c = $2 } END { print c }' "$BUILDS"
    fi
}

# short_id <image ID>: its first 12 hex digits, as docker prints them
short_id() {
    local id=${1#sha256:}
    echo "${id:0:12}"
}

# faucet_waiting <container>: the transfers waiting for a block in the faucet, by its GET / (signet/services/faucet.mjs)
# asked inside its container: their number, or "refused" while it does not listen, which it starts only once its node
# answers, before it takes any request. Fails when the faucet does not answer
faucet_waiting() {
    # shellcheck disable=SC2016
    docker exec "$1" node -e '
        fetch(`http://127.0.0.1:${process.env.PORT || 8081}/`, { signal: AbortSignal.timeout(10000) })
            .then((r) => {
                if (!r.ok) throw new Error(`HTTP ${r.status}`);
                return r.json();
            })
            .then((j) => {
                if (!Number.isInteger(j.waiting)) throw new Error(`no waiting count in ${JSON.stringify(j)}`);
                console.log(j.waiting);
            })
            .catch((e) => {
                if (e.cause?.code === "ECONNREFUSED") return console.log("refused");
                console.error(e.cause?.code ?? e.message);
                process.exitCode = 1;
            });'
}

# faucet_look: one look at the faucet, printing what it showed; fails while transfers wait for a block in it, which a
# restart would forget (they live in its memory, D-107), and while it does not answer
faucet_look() {
    local id state='' answer
    # A docker that does not answer here must not read as a faucet that does not run (set -e is off in this function)
    if ! id=$(container faucet) || { [ -n "$id" ] && ! state=$(docker inspect -f '{{.State.Status}}' "$id"); }; then
        echo "docker does not answer about its container"
        return 1
    fi
    case "$state" in
        running | paused) ;;
        restarting)
            # A faucet that fails as it starts: the fix this update brings must not wait for it
            echo "its process ended and Docker is starting it again, so no transfer waits in it"
            return 0
            ;;
        *)
            echo "it does not run, so no transfer waits in it"
            return 0
            ;;
    esac
    if ! answer=$(faucet_waiting "$id" 2>&1); then
        echo "it does not answer (${answer:-no reason given})"
        return 1
    fi
    case "$answer" in
        0) echo "no transfer waits for a block in it" ;;
        refused) echo "it does not listen yet (it waits for its node), so it has taken no request" ;;
        *)
            echo "$answer transfer(s) wait for a block in it"
            return 1
            ;;
    esac
}

# faucet_idle <minutes>: looks at the faucet every 10 seconds until no transfer waits in it; fails after <minutes>
faucet_idle() {
    local minutes=$1 start seen last=''
    start=$(date +%s)
    while ! seen=$(faucet_look); do
        if [ $(($(date +%s) - start)) -ge $((minutes * 60)) ]; then
            log "ERROR: after $minutes minute(s) the faucet still shows that $seen. It was not restarted, as a restart" \
                "forgets the transfers waiting in it: run this again later (the image is built already)"
            return 1
        fi
        if [ "$seen" != "$last" ]; then
            log "the faucet: $seen; looking again every 10 seconds, for at most $minutes minute(s): it is restarted" \
                "once no transfer waits in it"
            last=$seen
        fi
        sleep 10
    done
    log "the faucet: $seen"
}

# update_faucet <minutes>: the KSA faucet alone (deploy.sh faucet). Its image, ksana-signet:services, is the BTC proxy's
# too and is built through the btc-proxy service, which holds its build section (compose build faucet builds nothing);
# building it changes no container. Once no transfer waits in the faucet (for at most <minutes>), its container alone
# is recreated; the BTC proxy keeps its own on the image it runs, until a full deploy. Prints the commit the faucet then
# runs, alone on standard output
update_faucet() {
    local minutes=$1 id before='' image commit from proxy proxy_before='' proxy_after seen tries=0
    require_src
    log "src/ holds commit $(env_value KSANA_COMMIT)"
    id=$(container faucet)
    if [ -n "$id" ]; then
        before=$id
        image=$(docker inspect -f '{{.Image}}' "$id")
        commit=$(built_from "$image")
        from="built before the builds were recorded"
        [ -z "$commit" ] || from="built from commit $commit"
        log "the faucet runs the services image $(short_id "$image"), $from, since" \
            "$(docker inspect -f '{{.State.StartedAt}}' "$id")"
    else
        log "the faucet has no container yet"
    fi
    proxy=$(container btc-proxy)
    if [ -n "$proxy" ]; then
        proxy_before=$(docker inspect -f '{{.Id}} {{.Image}} {{.State.StartedAt}}' "$proxy")
        log "the BTC proxy runs the services image $(short_id "$(docker inspect -f '{{.Image}}' "$proxy")"): it keeps" \
            "its container and that image"
    fi
    log "building the services image (the faucet's and the BTC proxy's) through the btc-proxy service; no container changes"
    run compose build btc-proxy
    if [ "$DRY_RUN" = yes ]; then
        seen=$(faucet_look) || true
        log "the faucet now: $seen"
        log "would wait until no transfer waits in the faucet (at most $minutes minute(s)), then recreate its container alone"
        run compose up -d --no-deps --timeout 0 faucet
        run docker image prune -f
        return 0
    fi
    record_build ksana-signet:services
    faucet_idle "$minutes" || exit 1
    # Nothing in the faucet outlives it once no transfer waits, and its node process, PID 1 in the container, ignores
    # SIGTERM: with the default timeout it would go on taking requests for 10 seconds before it is killed
    compose up -d --no-deps --timeout 0 faucet >&2
    id=$(container faucet)
    image=$(docker inspect -f '{{.Image}}' "$id")
    # Compose recreates the container whenever the image's content or the service's settings differ from what it runs;
    # the image IDs cannot tell, as with the containerd image store every build gets a new one, the content unchanged
    if [ "$id" = "$before" ]; then
        log "the faucet kept its container: the services image just built has the content it runs, and its" \
            "settings are unchanged"
    else
        log "the faucet's container is recreated on the services image just built, $(short_id "$image")"
    fi
    if [ -n "$proxy" ]; then
        proxy_after=$(docker inspect -f '{{.Id}} {{.Image}} {{.State.StartedAt}}' "$proxy" 2>/dev/null || true)
        if [ "$proxy_after" = "$proxy_before" ]; then
            log "the BTC proxy kept its container and its image"
        else
            log "WARNING: the BTC proxy's container changed (before: $proxy_before; now: ${proxy_after:-none})"
        fi
    fi
    # It listens once its node answers
    until seen=$(faucet_waiting "$id" 2>&1) && [ "$seen" != refused ]; do
        tries=$((tries + 1))
        if [ "$tries" -ge 24 ]; then
            [ "$seen" != refused ] || seen="it does not listen yet, as it waits for its node"
            log "ERROR: two minutes on, the faucet does not answer (${seen:-no reason given}):" \
                "signet/deploy.sh <host> logs faucet shows why"
            exit 1
        fi
        sleep 5
    done
    log "the faucet answers: $seen transfer(s) waiting"
    run docker image prune -f >/dev/null
    commit=$(env_value KSANA_COMMIT)
    log "the faucet runs commit $commit"
    echo "$commit"
}

# backup_data <label> <restart: yes|no>: the data volumes of the seed nodes, the indexer and the points service (D-126),
# each a .tar.gz in backups/<label>-<UTC time>/ with their SHA-256 in SHA256SUMS. The services that use them stop first,
# with compose stop alone: their containers and volumes stay, and so does every other service, bitcoind first of all.
# They stay stopped for the deploy that follows to start them, unless restart is yes. Prints the directory's name alone
# on standard output
backup_data() {
    local label=$1 restart=$2 service id volume path name dir running=() volumes=() profile=()
    case "$label" in
        "" | [!A-Za-z0-9]* | *[!A-Za-z0-9._-]*)
            log "a label is letters, digits, '.', '_' and '-', and starts with a letter or a digit"
            exit 2
            ;;
    esac
    require_src
    for service in node1 node2 indexer points; do
        id=$(container "$service")
        if [ -n "$id" ] && [ "$(docker inspect -f '{{.State.Running}}' "$id")" = true ]; then
            running+=("$service")
        fi
    done
    for volume in node1-data node2-data indexer-data points-data; do
        if path=$(docker volume inspect -f '{{.Mountpoint}}' "ksana-signet_$volume" 2>/dev/null); then
            volumes+=("$volume")
            log "ksana-signet_$volume: $(du -sh "$path" | cut -f1) in $path"
        else
            log "ksana-signet_$volume: no such volume, nothing to save"
        fi
    done
    if [ ${#volumes[@]} -eq 0 ]; then
        log "no data volume to save"
        exit 1
    fi
    # A node2 that runs while .env says one producer, or a points service while .env names no points page, is stopped
    # too: compose needs its profile for that
    [ "$(env_value KSANA_SIGNET_PRODUCERS)" = 2 ] || profile=(--profile node2)
    [ -n "$(env_value KSANA_POINTS_ORIGIN)" ] || profile+=(--profile points)
    name=$label-$(date -u +%Y%m%dT%H%M%SZ)
    dir=$HOME_DIR/backups/$name
    if [ -e "$dir" ]; then
        log "$dir exists already"
        exit 1
    fi
    log "free on $HOME_DIR: $(df -h --output=avail "$HOME_DIR" | tail -1 | tr -d ' ')"
    if [ "$DRY_RUN" = yes ]; then
        if [ ${#running[@]} -gt 0 ]; then
            run compose "${profile[@]}" stop "${running[@]}"
        else
            log "none of node1, node2, the indexer and the points service runs: nothing to stop"
        fi
        log "would save ${volumes[*]} as .tar.gz in $dir, with SHA256SUMS"
        if [ "$restart" = yes ] && [ ${#running[@]} -gt 0 ]; then
            run compose "${profile[@]}" start "${running[@]}"
        fi
        echo "$name"
        return 0
    fi
    if [ ${#running[@]} -gt 0 ]; then
        log "stopping ${running[*]} (compose stop; bitcoind, the page, the BTC proxy and the faucet keep running)"
        compose "${profile[@]}" stop "${running[@]}" >&2
        for service in "${running[@]}"; do
            log "$service exited with status $(docker inspect -f '{{.State.ExitCode}}' "$(container "$service")")" \
                "(0 is a clean shutdown; 137 means killed when its stop_grace_period ran out)"
        done
    fi
    # Nothing may write a volume while it is saved
    for volume in "${volumes[@]}"; do
        if [ -n "$(docker ps -q --filter "volume=ksana-signet_$volume")" ]; then
            log "a running container still uses ksana-signet_$volume: not saving it"
            exit 1
        fi
    done
    umask 077
    mkdir -p "$HOME_DIR/backups"
    rm -rf "$dir.partial"
    # A backup cut short leaves nothing behind (the path is expanded now, while it is known)
    # shellcheck disable=SC2064
    trap "rm -rf $(printf %q "$dir.partial")" EXIT
    mkdir "$dir.partial"
    for volume in "${volumes[@]}"; do
        path=$(docker volume inspect -f '{{.Mountpoint}}' "ksana-signet_$volume")
        tar -C "$path" --numeric-owner -czf "$dir.partial/$volume.tar.gz" .
    done
    (cd "$dir.partial" && sha256sum -- *.tar.gz >SHA256SUMS && sha256sum --quiet -c SHA256SUMS)
    mv "$dir.partial" "$dir"
    trap - EXIT
    log "saved $dir: $(cd "$dir" && du -ch -- *.tar.gz | tail -1 | cut -f1) in all, checked against its SHA256SUMS"
    if [ ${#running[@]} -gt 0 ]; then
        if [ "$restart" = yes ]; then
            log "starting ${running[*]} again"
            compose "${profile[@]}" start "${running[@]}" >&2
        else
            log "${running[*]} stay stopped: the next deploy (signet/deploy.sh <host>, or up) starts them"
        fi
    fi
    echo "$name"
}

if [ "${1:-}" = --dry-run ]; then
    DRY_RUN=yes
    shift
fi
command=${1:-}
shift || true
case "$command" in
    build | web | faucet | playground | points | instances | backup-data) ;;
    *)
        if [ "$DRY_RUN" = yes ]; then
            log "--dry-run works with build, web, faucet, playground, points, instances and backup-data only"
            exit 2
        fi
        ;;
esac
case "$command" in
    check) check ;;
    setup) setup "$@" ;;
    commit)
        # the commits of src/: this repository's, ksana-web's (the page's build shows it) and ksana-points' (empty
        # without the points service)
        set_env KSANA_COMMIT "$1"
        set_env KSANA_WEB_COMMIT "${2:-}"
        set_env KSANA_POINTS_COMMIT "${3:-}"
        ;;
    build)
        require_src
        tag_prev
        log "building the images (the first build compiles the Rust binaries in release mode, about half an hour on 2 cores)"
        run compose build
        run docker image prune -f >/dev/null
        [ "$DRY_RUN" = yes ] || record_build ksana-signet:ksana ksana-signet:services
        ;;
    web)
        # The web page alone (deploy.sh web): its image is built from src/ and its container recreated; bitcoind, the
        # seed nodes, the indexer, the BTC proxy and the faucet are left as they are
        require_src
        log "src/ holds commit $(env_value KSANA_COMMIT), with ksana-web $(env_value KSANA_WEB_COMMIT)"
        run compose build web
        run compose up -d --no-deps web
        run docker image prune -f >/dev/null
        ;;
    faucet)
        # The faucet alone (deploy.sh faucet), once no transfer waits in it for at most <minutes> (default 10)
        case "${1:-10}" in
            "" | 0* | *[!0-9]*)
                log "the faucet's wait is a whole number of minutes"
                exit 2
                ;;
        esac
        update_faucet "${1:-10}"
        ;;
    playground)
        # The playground's compile service alone (deploy.sh playground, D-119): its image is built from src/ and its
        # container recreated; Caddy, the faucet and every other service are left as they are (the playground's page is
        # a Cloudflare Worker)
        require_src
        if [ -z "$(env_value KSANA_PLAYGROUND_ORIGIN)" ]; then
            log "no KSANA_PLAYGROUND_ORIGIN in $ENV_FILE: run signet/deploy.sh <host> setup --playground <name> first"
            exit 1
        fi
        log "src/ holds commit $(env_value KSANA_COMMIT)"
        log "building the playground's compile service (its first build builds every example once, a few minutes on 2 cores)"
        run compose build compiler
        run compose up -d --no-deps compiler
        run docker image prune -f >/dev/null
        ;;
    points)
        # The points service with the top-up seller, and the price keeper, alone (deploy.sh points, D-126): their image
        # is built from src/ and their containers recreated; Caddy and every other service are left as they are (the
        # points page is a Cloudflare Worker). The service reads its configuration, signet/points.json of ksana-points,
        # from the image
        require_src
        require_points
        log "src/ holds commit $(env_value KSANA_COMMIT), with ksana-points $(env_value KSANA_POINTS_COMMIT)"
        run compose build points
        run compose up -d --no-deps points keeper
        run docker image prune -f >/dev/null
        ;;
    instances)
        # The season's official instances (deploy.sh instances, D-126), deployed by the points image with the
        # operator's key, mounted for this run alone, for the accounts of the keeper and the top-up seller
        # (keys/keeper.pub, keys/seller.pub). The steps are kept in config/points/, one file per genesis block, so that
        # a run cut short goes on where it stopped; the backup takes them with the keys. Prints the instances as JSON,
        # alone on standard output
        require_src
        require_points
        if [ ! -s "$HOME_DIR/config/keys/keeper.pub" ] || [ ! -s "$HOME_DIR/config/keys/seller.pub" ] ||
            [ ! -f "$HOME_DIR/config/keys/operator.key" ]; then
            log "no keys/keeper.pub, keys/seller.pub or keys/operator.key: run signet/deploy.sh <host> keys first"
            exit 1
        fi
        mkdir -p "$HOME_DIR/config/points"
        chmod 700 "$HOME_DIR/config/points"
        extra=()
        [ "$DRY_RUN" = no ] || extra=(--dry-run)
        compose run --rm --no-deps -T --user root \
            -v "$HOME_DIR/config/keys/operator.key:/run/operator.key:ro" -v "$HOME_DIR/config/points:/state" \
            points instances --key /run/operator.key --keeper "$(cat "$HOME_DIR/config/keys/keeper.pub")" \
            --seller "$(cat "$HOME_DIR/config/keys/seller.pub")" --modules /app/wasm --state-dir /state "${extra[@]}"
        ;;
    domain)
        env_value KSANA_SIGNET_DOMAIN
        ;;
    playground-origin)
        env_value KSANA_PLAYGROUND_ORIGIN
        ;;
    points-origin)
        env_value KSANA_POINTS_ORIGIN
        ;;
    backup-data)
        backup_data "${1:-}" "${2:-no}"
        ;;
    btc)
        require_src
        compose up -d bitcoind
        ;;
    keys)
        require_src
        compose run --rm --no-deps ops keys
        ;;
    probe)
        require_src
        compose run --rm ops probe "$@"
        ;;
    funded)
        # Confirmed sats of node1's funding address
        require_src
        compose run --rm ops status | jq -r '.node1 | objects | .confirmed_sats // empty'
        ;;
    genesis)
        require_src
        compose run --rm ops genesis
        ;;
    up) up ;;
    status)
        require_src
        compose ps --format 'table {{.Service}}\t{{.Status}}'
        traffic
        compose run --rm ops status "$@"
        ;;
    logs)
        require_src
        compose logs --tail 200 -f "$@"
        ;;
    down)
        require_src
        compose down
        ;;
    genesis-file)
        cat "$GENESIS"
        ;;
    has-genesis)
        [ -f "$GENESIS" ] && echo yes || echo no
        ;;
    backup)
        tar -C "$HOME_DIR" -cz .env config
        ;;
    reset)
        # A restart from genesis: the Ksana chain data and the genesis config go, bitcoind and the keys stay
        require_src
        compose down
        for v in node1-data node2-data indexer-data; do
            docker volume rm -f "ksana-signet_$v" >/dev/null
        done
        rm -f "$GENESIS"
        log "removed the chain data of the nodes and the indexer, and the genesis config; the keys and bitcoind stay"
        ;;
    *)
        echo "usage: server.sh [--dry-run] check|setup|commit|build|web|faucet [<minutes>]|playground|points|instances|btc|keys|probe|funded|genesis|up|status|logs|down|genesis-file|has-genesis|domain|playground-origin|points-origin|backup|backup-data|reset" >&2
        exit 2
        ;;
esac
