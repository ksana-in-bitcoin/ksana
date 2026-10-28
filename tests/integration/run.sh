#!/usr/bin/env bash
# Runs the integration tests on the devnet's regtest bitcoind:
# build the TypeScript SDK and ksana-cli → docker compose up (waiting for the health check) → cargo test -p
# ksana-integration-tests → docker compose down -v. The M13 top-up end-to-end test (tests/m13_regtest.rs) runs
# target/debug/ksana-cli (KSANA_CLI can override it).
# Extra arguments are passed to cargo test as they are, e.g.: tests/integration/run.sh max_size
# The M6 end-to-end test (tests/m6_regtest.rs) runs the built SDK and the mock wallet with Node.js (≥ 20), and checks
# the wallet demo page with playwright-core's Chromium (first run npx playwright-core install --only-shell chromium in
# sdk/ts; KSANA_SKIP_BROWSER=1 skips the page check).
set -euo pipefail

cd "$(dirname "$0")/../.."
compose=(docker compose -f devnet/docker-compose.yml)

(cd sdk/ts && npm ci --no-audit --no-fund && npm run build && npm run build:demo)
cargo build -p ksana-cli

"${compose[@]}" up -d --wait
trap '"${compose[@]}" down -v' EXIT

cargo test -p ksana-integration-tests -- --test-threads=1 "$@"
