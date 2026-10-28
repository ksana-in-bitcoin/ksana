# Images of the hosted public signet (D-107), built on the server by signet/deploy.sh through
# signet/docker-compose.yml. The build context is the tree signet/sources.sh assembles (see .dockerignore): this
# repository, with the web wallet's repository at web/ and the testnet points' at points/, each with the part of this
# repository it pins as its core submodule under core/.
#
#   ksana       ksana-node, ksana-indexer, ksana-cli, ksana-keytool, the relay probe
#               (crates/ksana-btc/examples/relay_probe.rs) and the operator script signet/ksana-signet.sh
#   web         Caddy with the web wallet (ksana-web) built in: serves the page and proxies /rpc, /indexer, /btc,
#               /faucet, /compile and /points
#   services    Node.js with the TypeScript SDK (sdk/ts) and signet/services: the BTC proxy and the KSA faucet
#   compiler    the contract playground's compile service (playground/compiler, D-119): the pinned Rust toolchain, the
#               crates the contracts use, and a warm copy of the contracts workspace to build in (the playground's page
#               is a Cloudflare Worker, playground/wrangler.jsonc)
#   points      the testnet points service with the top-up seller, and the price keeper (ksana-points, D-126): one
#               bundled module, the signet's points configuration (signet/points.json of ksana-points) and the modules
#               of the season's official instances (its page is a Cloudflare Worker, wrangler.jsonc of ksana-points)
#
#   signet/sources.sh dir /tmp/ksana-signet-src && cd /tmp/ksana-signet-src
#   docker build -f signet/Dockerfile --target ksana -t ksana-signet:ksana .
#
# The Rust toolchain is the one rust-toolchain.toml pins; the web page is built with Node.js 22 like ksana-web's CI and
# with the SDK of the core commit ksana-web pins, so that the page's commit gives the same files (ksana-web's README,
# "Verify this page"; KSANA_COMMIT names that commit, as .git is not in the build context).

FROM rust:1.98.1-bookworm AS rust-build
# librocksdb-sys generates its bindings with bindgen, which needs libclang
RUN apt-get update \
    && apt-get install -y --no-install-recommends clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# The registry and the target directory are cache mounts: a deploy after a small change rebuilds only what changed
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ksana-node -p ksana-indexer -p ksana-cli --bins \
    && cargo build --release --locked -p ksana-btc --example relay_probe \
    && install -m 0755 target/release/ksana-node target/release/ksana-keytool \
        target/release/ksana-indexer target/release/ksana-cli /usr/local/bin/ \
    && install -m 0755 target/release/examples/relay_probe /usr/local/bin/ksana-relay-probe

FROM debian:bookworm-slim AS ksana
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust-build /usr/local/bin/ksana-node /usr/local/bin/ksana-keytool /usr/local/bin/ksana-indexer \
    /usr/local/bin/ksana-cli /usr/local/bin/ksana-relay-probe /usr/local/bin/
COPY --chmod=0755 signet/ksana-signet.sh /usr/local/bin/ksana-signet
# Node JSON-RPC, P2P and indexer JSON-RPC
EXPOSE 8545 9000 8546
ENTRYPOINT ["ksana-signet"]

# The SDK, built for the services
FROM node:22-bookworm-slim AS sdk-build
WORKDIR /src
COPY sdk/ts/package.json sdk/ts/package-lock.json sdk/ts/
RUN cd sdk/ts && npm ci --no-audit --no-fund
COPY sdk/ts sdk/ts
RUN cd sdk/ts && npm run build

# The web page, built as a checkout of ksana-web builds it: first the SDK of the core commit it pins (core/sdk/ts),
# then the page
FROM node:22-bookworm-slim AS js-build
WORKDIR /src/web
COPY web/core/sdk/ts/package.json web/core/sdk/ts/package-lock.json core/sdk/ts/
RUN cd core/sdk/ts && npm ci --no-audit --no-fund
COPY web/core/sdk/ts core/sdk/ts
RUN cd core/sdk/ts && npm run build
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web ./
ARG KSANA_COMMIT=""
RUN KSANA_COMMIT="$KSANA_COMMIT" npm run build

# The SDK's runtime dependencies only
FROM node:22-bookworm-slim AS sdk-deps
WORKDIR /src/sdk/ts
COPY sdk/ts/package.json sdk/ts/package-lock.json ./
RUN npm ci --omit=dev --no-audit --no-fund

FROM caddy:2.11.7-alpine@sha256:d8542f48d34a9cf4e4c11a478865229840e87e4c96ea3f439101f31a5d35f75f AS web
COPY --from=js-build /src/web/dist /srv
COPY signet/deployment.json /srv/deployment.json
COPY signet/Caddyfile /etc/caddy/Caddyfile

FROM node:22-bookworm-slim AS services
WORKDIR /app
COPY --from=sdk-deps /src/sdk/ts/package.json sdk/ts/package.json
COPY --from=sdk-deps /src/sdk/ts/node_modules sdk/ts/node_modules
COPY --from=sdk-build /src/sdk/ts/dist/src sdk/ts/dist/src
COPY signet/services signet/services
USER node
ENTRYPOINT ["node"]

# The points service (ksana-points, D-126): one module, dist-server/points.mjs, with the SDK, the playground's ABI
# parser and the examples' sources bundled in; only Node's own modules stay outside. Built as a checkout of
# ksana-points builds it, with the core commit it pins (points/core/)
FROM node:22-bookworm-slim AS points-build
WORKDIR /src/points
COPY points/core/sdk/ts/package.json points/core/sdk/ts/package-lock.json core/sdk/ts/
RUN cd core/sdk/ts && npm ci --no-audit --no-fund
COPY points/core/sdk/ts core/sdk/ts
RUN cd core/sdk/ts && npm run build
COPY points/package.json points/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY points ./
RUN npm run build:server

# The season's poll and exchange (contracts/examples/poll, contracts/examples/ksa-exchange), which the playground does
# not ship: built for wasm32 as contracts/Cargo.toml says, inside contracts/ with the pinned toolchain, from the core
# commit ksana-points pins
FROM rust:1.98.1-bookworm AS contracts-build
WORKDIR /src
COPY points/core/Cargo.toml points/core/rust-toolchain.toml ./
COPY points/core/crates/ksana-contract-sdk/Cargo.toml crates/ksana-contract-sdk/Cargo.toml
COPY points/core/crates/ksana-contract-sdk/src crates/ksana-contract-sdk/src
COPY points/core/contracts contracts
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cd contracts \
    && cargo rustc --release --locked --target wasm32-unknown-unknown --package poll --crate-type cdylib \
    && cargo rustc --release --locked --target wasm32-unknown-unknown --package ksa-exchange --crate-type cdylib \
    && install -D -m 0644 target/wasm32-unknown-unknown/release/poll.wasm /out/poll.wasm \
    && install -D -m 0644 target/wasm32-unknown-unknown/release/ksa_exchange.wasm /out/ksa-exchange.wasm

# The points service and the price keeper (D-126). /app/wasm holds the modules the official instances are deployed
# from (node points.mjs instances): the token, the AMM and the lending market the playground ships, the poll and the
# exchange. The service keeps its database in /data, a volume
FROM node:22-bookworm-slim AS points
WORKDIR /app
COPY --from=points-build /src/points/dist-server/points.mjs points.mjs
COPY points/signet/points.json points.json
COPY points/core/playground/src/examples/wasm/mintable-token.wasm points/core/playground/src/examples/wasm/amm.wasm \
    points/core/playground/src/examples/wasm/lending.wasm wasm/
COPY --from=contracts-build /out/poll.wasm /out/ksa-exchange.wasm wasm/
RUN install -d -o node -g node /data
USER node
ENV KSANA_POINTS_CONFIG=/app/points.json \
    KSANA_POINTS_DB=/data/points.db
EXPOSE 8082
# node:sqlite is still marked experimental in Node 22
ENTRYPOINT ["node", "--disable-warning=ExperimentalWarning", "/app/points.mjs"]

# The playground's compile service (playground/compiler, D-119). The toolchain is rust-toolchain.toml's, with its
# components and the wasm32 target installed here as root; the crates the contracts use are fetched here, so builds run
# offline; the slots (copies of the contracts workspace to build in) are made and warmed here by a build of every
# example, as the user the service runs as. The image holds no secret: a source can read any file the user can
# (include_bytes!) and the variables of its build (env!), which carry none
FROM rust:1.98.1-bookworm AS compiler
COPY --from=node:22-bookworm-slim /usr/local/bin/node /usr/local/bin/node
COPY Cargo.toml rust-toolchain.toml /opt/ksana/
COPY crates/ksana-contract-sdk/Cargo.toml /opt/ksana/crates/ksana-contract-sdk/Cargo.toml
COPY crates/ksana-contract-sdk/src /opt/ksana/crates/ksana-contract-sdk/src
COPY contracts /opt/ksana/contracts
RUN cd /opt/ksana/contracts && rustc --version \
    && useradd --system --create-home --uid 10001 compiler \
    && install -d -o compiler /var/lib/compiler
COPY playground/compiler /opt/compiler
ENV CARGO_HOME=/var/lib/compiler/cargo \
    KSANA_COMPILER_REPO=/opt/ksana \
    KSANA_COMPILER_SLOTS_DIR=/var/lib/compiler/slots
USER compiler
RUN cargo fetch --locked --manifest-path /opt/ksana/contracts/Cargo.toml
ARG KSANA_COMPILER_SLOTS=1
ENV KSANA_COMPILER_SLOTS=$KSANA_COMPILER_SLOTS
# every example's own source must build into the module the playground ships
COPY playground/src/examples/wasm/builds.json /opt/ksana/playground/src/examples/wasm/builds.json
RUN KSANA_COMPILER_SHIPPED=/opt/ksana/playground/src/examples/wasm/builds.json node /opt/compiler/server.mjs --prepare
ENV KSANA_COMPILER_HOST=0.0.0.0 \
    KSANA_COMPILER_PORT=8090
EXPOSE 8090
ENTRYPOINT ["node", "/opt/compiler/server.mjs"]
