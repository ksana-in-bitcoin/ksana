# Ksana devnet image (D-042): multi-stage build; the runtime stage contains ksana-node, ksana-keytool, ksana-indexer
# (D-048), ksana-cli (D-051) and the devnet initialization script (needs curl, jq).
#
#   docker build -f devnet/Dockerfile -t ksana-node:devnet .
#
# The build context is the repository root (see .dockerignore). The toolchain version is locked by
# rust-toolchain.toml and matches the base image.

FROM rust:1.98.1-bookworm AS build
# librocksdb-sys generates its bindings with bindgen, which needs libclang
RUN apt-get update \
    && apt-get install -y --no-install-recommends clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p ksana-node -p ksana-indexer -p ksana-cli --bins \
    && install -m 0755 target/release/ksana-node target/release/ksana-keytool \
        target/release/ksana-indexer target/release/ksana-cli /usr/local/bin/

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/ksana-node /usr/local/bin/ksana-keytool \
    /usr/local/bin/ksana-indexer /usr/local/bin/ksana-cli /usr/local/bin/
COPY devnet/init.sh /usr/local/bin/ksana-devnet
# Node JSON-RPC, P2P and indexer JSON-RPC
EXPOSE 8545 9000 8546
ENTRYPOINT ["ksana-node"]
