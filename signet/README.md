# Hosted public signet: operator guide (D-107)

Ksana's public test network runs on **Bitcoin's public signet**. This directory deploys its hosted part to one server: the
seed nodes everyone connects to, an indexer, the web wallet with a public RPC, a KSA faucet, the contract playground's
compile service (D-119) and the testnet points service (D-126), whose pages Cloudflare serves at domains of their own.
Everyone else joins with `docs/SIGNET.md`.
Every command below runs on the operator's machine (macOS or Linux) and reaches the server over ssh with the operator's own
keys.

| Service | What it is | Public as |
| --- | --- | --- |
| `bitcoind` | Bitcoin Core 31.1 on the public signet, `-txindex`, connected to the signet miners' own node | P2P `:38333` |
| `node1` | seed node: produces blocks (Argon2id on one core) and settles them on signet | P2P `:9000`, RPC `https://<domain>/rpc` |
| `node2` | second producing seed node, only with `--producers 2` (4 cores or more); the faucet sends through it | P2P `:9001` |
| `indexer` | rebuilds the chain from signet alone and checks every `state_root` (D-048) | `https://<domain>/indexer` |
| `btc-proxy` | four bitcoind methods the wallet needs for path C and top-ups (not `scantxoutset`), and two paths of mempool.space's signet API for listing an address's coins (`services/btc-proxy.mjs`) | `https://<domain>/btc` |
| `faucet` | 100 KSA per account a day from the genesis allocation (`services/faucet.mjs`) | `https://<domain>/faucet` |
| `web` | Caddy: the web wallet (the ksana-web repository), the proxies above and below, the certificate | `https://<domain>/` |
| `compiler` | the contract playground's compile service (`playground/compiler`), on an internal network only `web` reaches; only once the playground's page has an origin (`setup --playground`), which it and the faucet allow | `https://<domain>/compile` |
| `points` | the testnet points service (the ksana-points repository): scans the chain, credits the season's tasks at BTC-final, answers the points page and sells the top-ups of the top-up task; only once that page has an origin (`setup --points`), which it alone allows | `https://<domain>/points` |
| `keeper` | the official lending market's price keeper (`server/keeper.ts` of ksana-points), with the `points` service | none |

The page is the build of ksana-web at the commit its footer shows (verifiable per ksana-web's README, "Verify this page")
plus `deployment.json`, which makes a first-time visitor land on this signet instead of the simulator. The project's own
deployment answers at `https://signet.ksana.network`, with its seed node at `/dns4/signet.ksana.network/tcp/9000`; its
former domain still answers (below, D-112).

## The sources

The web wallet and the testnet points live in repositories of their own, ksana-web and ksana-points (private), each with
this repository as its `core` submodule. `deploy.sh` pushes, and `rehearse.sh` builds, what `signet/sources.sh` puts
together: the committed `HEAD` of this repository, of ksana-web with the SDK of the core commit it pins (so the page is the
build its commit names), and, when the server runs the points service, of ksana-points with the core commit it pins.
Uncommitted changes are left out, with a note. Clone them beside this repository, or name their checkouts with
`KSANA_WEB_DIR` and `KSANA_POINTS_DIR`:

```sh
git clone --recurse-submodules https://github.com/ksana-in-bitcoin/ksana-web ../ksana-web
git clone --recurse-submodules https://github.com/ksana-in-bitcoin/ksana-points ../ksana-points   # with the points service
```

The commit a sibling pins must be in this clone or in the sibling's own `core/` checkout. The server's `.env` records the
three commits (`KSANA_COMMIT`, `KSANA_WEB_COMMIT`, `KSANA_POINTS_COMMIT`).

## The server

Ubuntu 22.04 or 24.04, 2 cores or more (not burstable), 4 GB of memory or more, 80 GB of disk, a public IPv4 address,
reachable with `ssh <host>` as root or as a user with passwordless sudo. Open ports 22, 80, 443, 9000 (and 9001 with a second
producer, 38333 to serve signet peers), in the cloud provider's firewall too. A domain whose A record points at the server is
optional: without one the site is `<ip-with-dashes>.sslip.io`.

What it uses: bitcoind with `-txindex` takes most of the disk (the signet chain, about 30 GB) and downloads as much once,
while its upload to other signet nodes stays under about 1 GB a day (`-maxuploadtarget`); the rest stays under 2 GB of
memory. The first build compiles the Rust binaries in release mode, about half an hour on 2 cores; `setup` adds 4 GB of swap
for it. Each producing seed node keeps one core busy. Nothing caps the seed nodes' P2P or the HTTPS traffic: on a server with a
monthly traffic package, set the provider's traffic alarm (`status` prints the traffic since the server booted).

Settling costs signet BTC, paid by the seed node that produced the block carrying the settlement. Even an idle network
sends a settlement pair about every two signet blocks (each block that unlocks rewards has a body, D-109), 316 to 383 vB
plus a 240-sat anchor output each: about 0.0007 sBTC a day for the network at 2 sat/vB, the seed nodes' floor, up to
twice that while there is traffic. The nodes pay the floor while signet's mempool holds at most half a block, and
bitcoind's fee estimate when it holds more; a pair left waiting two blocks is bumped to the estimate, or to twice its
own rate and more while the mempool has room (D-117), up to the seed nodes' cap of 5 sat/vB: about 0.0015 sBTC a day for
the network when every pair pays it. The floor is not signet's minimum of 1 sat/vB: on 2026-10-13 signet's blocks took
nothing below 2 while a backlog at 1 sat/vB waited (D-107). The cap is not the node's default of 100: from 2026-10-16,
before D-117, the server's bitcoind estimated 100.03 sat/vB for every target up to 24 blocks while signet's blocks took
transactions at 1 sat/vB, and every pair paid 100, about 0.022 sBTC a day (D-107). Fund each producing seed node's
address with at least three outputs: 0.006 sBTC in all to start, 17 to 20 days of an idle network when two seed nodes
share it (about nine days for one alone), and 0.02 recommended, four weeks with traffic. The nodes never split their
outputs, and every settlement returns its change to the address.

## First deploy

```sh
signet/deploy.sh <host> check                     # the system, the cloud region, and what the server reaches
signet/deploy.sh <host> setup --domain <domain>   # once; without --domain the sslip.io name
signet/deploy.sh <host>                           # setup, push, build, keys, up
```

`check` changes nothing: it prints the system, the cores, memory, disk and swap, the CPU time the hypervisor takes, the
region of a Tencent Cloud machine, and how the server reaches Docker's packages and images, crates.io, GitHub, Debian and
npm, Let's Encrypt and two signet peers. A server in mainland China serves a domain on ports 80 and 443 only with an ICP
filing, so pick a region outside it (Hong Kong, Singapore).

The domain needs one DNS record: an `A` record for the site's name pointing at the server, not proxied (on Cloudflare,
"DNS only"). Caddy then takes a Let's Encrypt certificate on ports 80 and 443, and the same name serves the seed node's
P2P port. `setup --domain` refuses a name that does not resolve to the server alone, before it saves anything, so that Caddy
does not ask for a certificate it cannot get; a changed record shows once the server's resolver drops the old one (the
record's TTL). Leave out an `AAAA` record: the services have no IPv6, so every IPv6 visitor would share one address in the
faucet's and the BTC proxy's limits (`setup` warns about one).

`deploy.sh` keeps its ssh connection alive through the quiet minutes of a build (`ServerAliveInterval` 30); a proxy or VPN
that still drops it only interrupts the command, and running it again goes on.

The first run installs Docker, copies the sources (above) to `/opt/ksana-signet/src`, builds the images, creates the
keys in `/opt/ksana-signet/config/keys` and starts bitcoind, which syncs the signet chain (a few hours). It prints the seed
nodes' funding addresses and the probe's address: send signet BTC to each (any signet faucet): to each producing seed node a
few outputs as above, to the probe 0.005 sBTC (one run of its five tests at up to 5 sat/vB, the rate it paid on 2026-10-13,
below). bitcoind finds their coins through its watch-only wallets `ksana-node1`, `ksana-node2` and `ksana-probe`, which
`keys` creates as soon as bitcoind answers, so the coins sent while it syncs are found too (D-109). Every later deploy keeps
the keys, the wallets and bitcoind's chain.

### A new domain

When the signet moves to another domain, the old one stays as its former domain (`--former`, `signet/Caddyfile`, D-112):
Caddy keeps a certificate for it, `/rpc`, `/indexer`, `/btc` and `/faucet` answer there as before, and every other path
moves to the domain for good (301, path and query kept). Its `A` record keeps pointing at the server, DNS only like the
domain's. The project's own deployment:

```sh
signet/deploy.sh <host> setup --domain signet.ksana.network --former <the former domain>
signet/deploy.sh <host> web      # only the web service reads the names: its container is recreated with them
```

`setup` checks the former domain as it checks the domain and refuses the domain as its own former one; `--former none`
drops it. The endpoints stay where they were because their clients cannot follow a redirect: the CLI's JSON-RPC client
follows none, `fetch` (the SDK) repeats a `POST` moved by a 301 as a `GET`, WebSocket clients follow none, and a browser's
cross-origin call stops at a redirected preflight. What a visitor's browser keeps for the page (the theme, the language, the
data source, the wallet last connected, recent recipients) belongs to the old origin, and the browser wallet asks again to
connect to the new one; no key is lost, as the page keeps none.

The project's site and docs at `ksana.network` (and `www.ksana.network`) are not served from this server: Cloudflare Workers
serve them (D-112), and the contract playground's page at `playground.ksana.network` too (D-119). `signet.ksana.network` is
the name of the zone that points here.

### The contract playground

The contract playground (`playground/`, D-119) has its page on Cloudflare and its compile service here. A Cloudflare Worker
serves the page at `playground.ksana.network` (`playground/wrangler.jsonc`: static files only, as for the project's site), and
the page calls node1's `/rpc`, the faucet's `/faucet` and the compile service's `/compile` on this server across origins. Its
name needs no record pointing here: the Worker's custom domain makes its DNS record and certificate.

The server first, once, so that the page finds the compile service and the faucet answering its origin when it goes live:

```sh
signet/deploy.sh <host> setup --playground playground.ksana.network
signet/deploy.sh <host>          # the default deploy: the compile service's image is built, Caddy takes /compile, and the
                                 # faucet and the compile service allow the page's origin
```

Then the page, once: the Worker `ksana-playground` connected to the repository in the Cloudflare dashboard, as the site's is
(the README of ksana-site, "Deploying"), with the root directory `playground`, the build command `npm run build:hosted`, the deploy
command `npx wrangler deploy` and the build watch paths `playground/*`, `sdk/ts/*` and `contracts/examples/*` (the page reads
the examples' sources). Workers Builds then deploys it at every such push to `main`, and the deploy attaches
`playground.ksana.network`. A deploy from a machine runs `npm ci` and `npx wrangler deploy` in `playground/` with the token the
site's deploy uses.

`setup` saves `https://playground.ksana.network` as `KSANA_PLAYGROUND_ORIGIN` (`--playground none` turns the playground off
again). It turns on the Compose project's `playground` profile, the `compiler` container that builds what visitors compile, and
lets the page's origin, and no other, call the faucet and the compile service; node1 allows any origin. The first build of the
compiler's image takes a while: it installs the pinned Rust toolchain, fetches the crates the contracts use and builds every
example once in its slot, so that builds afterwards are quick and need no network; a build whose module is not the one the
playground ships (`playground/src/examples/wasm/builds.json`) stops the image's build. The compiler holds no secret: a source
it builds can read the files its user can and the variables of its build, so nothing but the repository's public files is
there. Its container has no capabilities, at most 512 processes, 2 GB of memory and one core, and no route out
(`signet/docker-compose.yml`). A build needs no more than one core, and its low CPU weight lets a busy server run the seed
nodes' mining, bitcoind and the indexer first.

The page's `deployment.json` is `playground/hosted/deployment.json`: the signet's endpoints, the explorer its pages link to
(the signet's page), and `instances`, contracts the operator deployed for everyone to use, as JSON
(`{"amm": "<contract ID>"}`), which an example's page lists first and opens for a visitor who has none of their own, with
`genesis`, the hash of the genesis block of the chain they are on (`ksana_getBlock` at height 0): the page offers them only on
that chain, so after a `reset` they are hidden until the file names the new chain's contracts and genesis block. A change to it
is a commit, which Workers Builds deploys with the page.

### The testnet points

The testnet points (D-126) live in their own repository, ksana-points, which is private: a Cloudflare Worker serves their
page, and their service, with the top-up seller and the official lending market's price keeper, runs here in the Compose
project's `points` profile. `setup --points <the page's domain>` saves the page's origin as `KSANA_POINTS_ORIGIN` and turns
the profile on (`--points none` turns it off again); from then on every push takes ksana-points too (above), `keys` writes
the keeper's and the top-up seller's keys with the others, and `points`, `instances` and `backup-data` operate the service.
ksana-points' README is their guide. `instances` writes the season's official instances into `signet/points.json` of
ksana-points, which names them to the service, and into `playground/hosted/deployment.json` here, which offers them on the
playground's pages: commit both, then run `points`.

### Inclusion check

Before the network starts, check that signet's miners mine both of Ksana's data carriers: the settlement envelope in a
reveal transaction's witness (D-081) and path C's large `OP_RETURN` (D-032). Once bitcoind has synced:

```sh
signet/deploy.sh <host> probe
```

The probe (`crates/ksana-btc/examples/relay_probe.rs`) sends settlement-shaped commit / reveal pairs with 600-byte and
81 920-byte envelopes and transactions with 220, 4 096 and 65 536-byte `OP_RETURN` outputs, follows signet for 6 blocks and
prints where each was mined. Exit status 0 means all were mined. A reveal left out while its commit was mined means the
envelope is filtered; a data transaction refused by our own bitcoind, or never mined, means its `OP_RETURN` is. Bitcoin
Inquisition, which signet's miners run, relays both by default since 29.1, like Bitcoin Core since 30.0; whether to move to a
self-hosted signet if they do not is the project owner's decision. Its payloads start with `0xff`, which no node reads.

The probe pays bitcoind's fee estimate for confirmation within 2 blocks, at least 2 sat/vB, plus 1 (`--feerate auto`, the
default; `--feerate <sat/vB>` sets a rate), since a rate signet's miners do not take at the moment looks the same as a
filter. Nothing caps the automatic rate: while the estimate is far above what signet's blocks take, as it was from
2026-10-16 at 100.03 sat/vB (D-107), it would be 101 and the five tests about 0.095 sBTC, so set a rate then (the lowest
rates of signet's last blocks are on mempool.space). At 3 sat/vB the five tests need about 0.0029 sBTC and at 5 about
0.0047, two thirds of it for the 65 536-byte `OP_RETURN`: `--op-return 220,4096` leaves that one out (about 0.0009 sBTC
at 3 sat/vB), and `--envelope 600 --op-return 65536` runs it later with only the small envelope.

Each test spends an output of its own: the probe first splits its funds and waits for the split to confirm. A split that
waits too long is replaced at a higher rate: stop the probe (Ctrl-C), make sure its container on the server has stopped
too, and run it again with the split's txid, at the automatic rate once it has risen or with a higher `--feerate` (at the
split's own rate the probe would build the same split again, and refuses). A plain run would find no coins, since the
probe's wallet does not list the ones a waiting transaction spends; when the split was mined in the meantime, the probe
says so, and a run without `--replace` goes on from it.

```sh
ssh <host> 'sudo docker ps -q --filter name=ksana-signet-ops-run | xargs -r sudo docker stop'
signet/deploy.sh <host> probe --replace <the split's txid>
```

### Launch

After the probe passed and the seed nodes' funding has a confirmation:

```sh
signet/deploy.sh <host> launch
git add signet/genesis.json && git commit -m "M32: the signet's genesis config [skip ci]"
```

`launch` writes the genesis config anchored at the current signet tip with the timestamp of that moment (ASERT's anchor,
D-026: start the nodes right away, which `launch` does), starts every service and copies the config back to
`signet/genesis.json`, the file everyone joining needs. Its genesis allocations: 100 000 000 KSA to the faucet and 10 000 000
to the operator account; the block share (D-089) goes to a signet-only account. The genesis difficulty assumes Argon2id at
about 1 ms per hash on each producing core and a block every 5 seconds; ASERT corrects it from there. A `launch` cut short
after it wrote the genesis config goes on where it stopped when run again.

Until the first op the seed nodes produce empty blocks, whose weight doubles block by block from 12 up to 120 (D-090), so
they come further and further apart, toward one about every 10 minutes once ASERT has matched the seed nodes' hashrate.
Nothing is settled meanwhile: an empty block has no body, and rewards unlock only for blocks that have landed. The headers
land with the settlement of the first block with an op or, without one, when a seed node republishes them once the first
one's anchor lags the signet tip by more than 24 blocks (4 × `SETTLEMENT_TIMEOUT`, D-047), about four hours; until then
`status` shows nothing finalized and the indexer at the genesis block. A request to the faucet starts the settlements right
away.

The copy `launch` leaves in `signet/genesis.json` is what every later deploy compares with the server's config. When the
config is committed from another checkout instead (a pull request made from a copy of the file), move this copy aside
before pulling that commit, then compare the two: git refuses to overwrite an untracked file, or a changed tracked one after
a restart from genesis, even when the contents are the same.

```sh
mv signet/genesis.json /tmp/genesis.launch.json
git pull
cmp signet/genesis.json /tmp/genesis.launch.json   # silent when the committed config is the server's
```

## Every day

```sh
signet/deploy.sh <host> status         # bitcoind, the nodes and their funds, the indexer, the faucet, the traffic
signet/deploy.sh <host> logs node1      # any service; Ctrl-C stops following
signet/deploy.sh <host>                 # deploy the sources: rebuilds what changed and restarts
signet/deploy.sh <host> web             # a commit that changes only the page: rebuilds and restarts the web service alone
signet/deploy.sh <host> faucet          # a commit that changes only the faucet: rebuilds and restarts it alone, once idle
signet/deploy.sh <host> playground      # a commit that changes only the compile service: rebuilds and restarts it alone
signet/deploy.sh <host> points          # a commit that changes only ksana-points: the same for the points service
signet/deploy.sh <host> backup ~/ksana-signet-backups
```

A full deploy builds every image, and a new ksana image restarts the seed nodes and the indexer, even for a change to the
page alone. `web` is for such a commit (ksana-web, the SDK it pins, `signet/Caddyfile`, `signet/deployment.json`): it
pushes the sources, builds only the web image and recreates only the web container (`compose up -d --no-deps web`), so
bitcoind, the seed nodes, the indexer, the BTC proxy and the faucet keep running untouched. It ends by reading the commit
the live page reports, the one its footer shows, and fails when that is not the commit of ksana-web's `HEAD`.

`faucet` is for a commit that changes only the faucet (`signet/services/faucet.mjs`, the SDK it runs on). It pushes the
sources and builds the services image, which the faucet and the BTC proxy share, through the btc-proxy service that holds its build
section (`compose build faucet` builds nothing); building changes no container. The transfers waiting for a block live in
the faucet's memory, and a restart forgets them: one that leaves the node's pool would not be sent again, and the faucet
would count its nonces again from the ones blocks have included, which the waiting ones already took. So it then looks
at the faucet's `waiting` every 10 seconds until it is 0, for at most 10 minutes (`--wait <minutes>` sets another
limit); past that it stops with an error and leaves the faucet as it was, and running it again later finds the image
built. A faucet that does not listen yet, as it still waits for its node, has taken no request; one whose process keeps
failing as it starts holds none, and neither does one stopped by hand, which the update starts again. When Docker or
the faucet does not answer a look, or the faucet's answer is not a success, the wait goes on. Then it recreates the
faucet's container alone
(`compose up -d --no-deps --timeout 0 faucet`: its node process ignores the stop signal and would go on taking requests
for the 10 seconds of the default timeout) and leaves the BTC proxy on the image it runs, until the next full deploy. It
ends by printing the commit the faucet runs, which must be `HEAD`'s, and what `https://<domain>/faucet` answers.

`playground` is for a commit that changes only what the playground's compile service builds with (`playground/compiler`,
the example contracts, the SDK crate). It pushes the sources, builds the compiler's image and recreates its container alone
(`compose up -d --no-deps compiler`), then waits for `/compile/health` to answer, checks that a preflight from the page's
origin is allowed, and prints the compiler's Rust version and the build the page reports (`version.json`). A build waiting in
the old compiler is cut off by the restart; the page says so and the visitor compiles again. Workers Builds deploys the page
itself from `main`: a commit that changes an example reaches its page at the push and its compile service with `playground`.
A change to `signet/Caddyfile` needs `web`.

`points` is for a commit of ksana-points alone (its service, its `signet/points.json`, the core commit it pins). It pushes
the sources, builds the points image and recreates the `points` and `keeper` containers alone
(`compose up -d --no-deps points keeper`), then waits for `/points/health` to answer and checks that a preflight from the
page's origin is allowed. The service picks up its scan where it stopped.

A commit that changes anything else needs the full deploy. `--dry-run` after `web`, `faucet`, `playground`, `points`,
`backup-data` or a plain deploy reads the server and prints what would run.

A deploy refuses to go on when `signet/genesis.json` differs from the server's genesis config. Back up after launch: the
backup holds the keys of the faucet, the seed nodes and the block share account, and a new server can only continue this
network with them (`signet/deploy.sh <newhost> restore <file>`, then `signet/deploy.sh <newhost>`). Keep backups outside the
repository; git ignores `ksana-signet-*.tar.gz` in case one lands there.

Top up a seed node's address before it runs dry. `status` lists each seed node's funds from its wallet (`confirmed_sats`,
`pending_sats` of change still in the mempool, `outputs`) and sets `top_up` below 0.002 sBTC or three outputs: about six
days of an idle network's pairs at 2 sat/vB for one of two seed nodes, fewer with traffic. A seed node without funds keeps
producing, and the other one carries its blocks' data; with both dry nothing lands on signet, so finality and the indexer
stop while blocks go on, and `status` prints a `warning` saying so.

## A deploy that changes the nodes' data

A change to what the seed nodes or the indexer keep on disk can leave their data unreadable to the version before it (an
index built at the first start, for instance). Before deploying one, save their data volumes:

```sh
signet/deploy.sh <host> backup-data before-<change> --dry-run   # what it would stop and save
signet/deploy.sh <host> backup-data before-<change>             # stops node1, node2 and the indexer, saves their data
signet/deploy.sh <host>                                         # the deploy starts them again, on the new image
```

`backup-data` stops node1, node2, the indexer and the points service with `compose stop` alone: their containers and volumes
stay, and bitcoind, the page, the BTC proxy and the faucet keep running. It saves each data volume as a `.tar.gz` in
`/opt/ksana-signet/backups/<label>-<UTC time>/` with `SHA256SUMS`, copies that directory to `~/ksana-signet-backups/` on
this machine and checks it there too. The three services stay stopped for the deploy that follows; `--restart` starts
them again instead. The seed nodes' volumes hold their P2P keys (`p2p.key`, behind the peer IDs of their addresses): keep
the backups private.

Every build first tags the ksana image the seed nodes and the indexer run `ksana-signet:ksana-prev-<commit>`, after the
commit it was built from (`/opt/ksana-signet/ksana-builds` records each build; an image built before that record is named
by its image ID instead), so that the build's prune keeps it. The tags stay until removed with
`docker image rm ksana-signet:ksana-prev-<commit>` on the server.

To go back to the kept image and the saved data, with `<backup>` the directory `backup-data` named:

```sh
signet/deploy.sh <host> backup-data before-rollback   # saves the data as it is now too, and stops the three services
ssh <host> 'sudo bash -s' <<'EOF'
set -e
b=/opt/ksana-signet/backups/<backup>
cd "$b" && sha256sum -c SHA256SUMS
for f in "$b"/*.tar.gz; do
    d=$(docker volume inspect -f '{{.Mountpoint}}' "ksana-signet_$(basename "$f" .tar.gz)")
    find "$d" -mindepth 1 -delete
    tar -xzf "$f" -C "$d"
done
docker tag ksana-signet:ksana-prev-<commit> ksana-signet:ksana
EOF
signet/deploy.sh <host> up                            # node1, node2 and the indexer start on the kept image
```

A full deploy of a tree that still holds the change starts the new version again.

## Restarting from genesis

A consensus change before mainnet restarts signet and devnet from genesis (`CONTRIBUTING.md`):

```sh
signet/deploy.sh <host> reset --yes    # stops everything, deletes the Ksana chain data and the genesis config
signet/deploy.sh <host>                # deploy the new code; only bitcoind runs
signet/deploy.sh <host> launch         # a new genesis config at the current tip; commit it
```

bitcoind's chain and every key stay, so the faucet and the seed nodes keep their accounts and addresses. The points service
keeps what it credited: it closes the old chain and follows the new one, whose official instances `instances` deploys again
(their steps are kept per genesis block).

## Rehearsal

`signet/rehearse.sh` runs this whole stack on the local machine, built from the sources (above), with bitcoind on a private signet that a miner service
extends every 15 seconds (`rehearsal.yml`), and checks it through the page's own origin: keys and wallets, genesis, node1
funding itself from its wallet, blocks settled on BTC, the indexer agreeing with the node, the page and `deployment.json`, the
BTC proxy, a faucet transfer arriving, and the relay probe. It runs the contract playground's compile service too, at
`/compile`: its health, a build of the counter's own source that must give the module the playground ships, byte for byte,
and a broken source whose error must name its line. It checks that node1 allows any origin and that the faucet and the
compile service allow the playground's page, here at the development server's origin (`http://localhost:5174`), and no other
page. It runs the points service with a configuration naming the rehearsal's faucet (`rehearsal.yml`): `/points` must allow
the points page's development origin (`http://localhost:5175`) alone and credit the faucet's transfer once it is BTC-final,
to an account that does not take part, having no X account linked (D-127); then the official instances are deployed as
`instances` does, twice, the second run deploying nothing, and the keeper must find the market following the pool. Last, a
top-up end to end: a buyer with a new key signs in to `/points`, is refused a lock until it links an X account, links one
through a stand-in for X's oEmbed endpoint that the script runs in the BTC proxy's container (`KSANA_POINTS_X_OEMBED` in
`rehearsal.yml`), and gets the seller's lock; `ksana-cli top-up` pays it from the rehearsal's wallet, and the service must
credit the top-up task once the claim is BTC-final and count the buyer as the one account taking part. It needs Docker and
the two sibling checkouts; `KSANA_REHEARSAL_KEEP=1` leaves it running on http://127.0.0.1:18080, where the playground's
development server can use it (`playground/README.md`).

## To watch on the first deploy

- The page's coin listing, which depends on mempool.space: the BTC proxy refuses `scantxoutset`, which takes 30 to 50
  seconds a scan on signet (D-107, D-109), and the seed nodes and the probe list their coins from their wallets.
- The seed nodes' funds (`top_up` in `status`), and the traffic (`status`) against the package.
- The build's time and memory, the compiler's first build included.
- The compile service's load (`logs compiler`): one build at a time, at most 8 waiting, 6 a minute from one address.
