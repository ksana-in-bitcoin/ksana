#!/usr/bin/env node
// The playground's compile service (playground/compiler/README.md): builds a contract's source as one of the example
// crates and answers with the module and rustc's diagnostics. It holds no secret and reaches no network; the hosted
// signet runs it in a container of its own behind Caddy (signet/docker-compose.yml, D-119).
//
//   node playground/compiler/server.mjs             serve on 127.0.0.1:8090, making its slots first if they are missing
//   node playground/compiler/server.mjs --prepare   make the slots and build every example once in each, then exit
//
// Settings (environment):
//   KSANA_COMPILER_HOST, KSANA_COMPILER_PORT   where it listens (127.0.0.1, 8090)
//   KSANA_COMPILER_REPO                        the repository the slots are made from (the one holding this file)
//   KSANA_COMPILER_SLOTS_DIR                   where the slots live ($TMPDIR/ksana-compiler)
//   KSANA_COMPILER_SLOTS                       how many builds run at once, a slot each (1)
//   KSANA_COMPILER_QUEUE                       how many builds may wait for a slot (8)
//   KSANA_COMPILER_TIMEOUT_SECS                when a build is stopped (120)
//   KSANA_COMPILER_PER_MINUTE                  builds a minute per address, 0 for no limit (6; as many at once, up to 10)
//   KSANA_COMPILER_TRUST_PROXY=1               take the address from X-Forwarded-For (behind Caddy, which sets it)
//   KSANA_COMPILER_CORS                        the origin of a page that calls it from another origin (the hosted
//                                              signet's: the playground's page; none: only pages that proxy it)
//   KSANA_COMPILER_SHIPPED                     with --prepare: the playground's builds.json, which every example's build
//                                              must equal (playground/src/examples/wasm/builds.json)
import { readFileSync } from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Builder, prepareSlot } from './lib/build.mjs';
import { createHandler, Limiter, Pool } from './lib/service.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const CRATES = JSON.parse(readFileSync(path.join(here, 'crates.json'), 'utf8'));

function number(name, fallback, min = 0) {
  const raw = process.env[name];
  if (raw === undefined || raw === '') return fallback;
  const n = Number(raw);
  if (!Number.isInteger(n) || n < min) throw new Error(`${name} must be a whole number of at least ${min}, not ${raw}`);
  return n;
}

const config = {
  host: process.env.KSANA_COMPILER_HOST || '127.0.0.1',
  port: number('KSANA_COMPILER_PORT', 8090, 1),
  repo: path.resolve(process.env.KSANA_COMPILER_REPO || path.join(here, '..', '..')),
  slotsDir: path.resolve(process.env.KSANA_COMPILER_SLOTS_DIR || path.join(os.tmpdir(), 'ksana-compiler')),
  slots: number('KSANA_COMPILER_SLOTS', 1, 1),
  queue: number('KSANA_COMPILER_QUEUE', 8),
  timeoutMs: number('KSANA_COMPILER_TIMEOUT_SECS', 120, 10) * 1000,
  perMinute: number('KSANA_COMPILER_PER_MINUTE', 6),
  trustProxy: process.env.KSANA_COMPILER_TRUST_PROXY === '1',
  cors: process.env.KSANA_COMPILER_CORS || '',
  shipped: process.env.KSANA_COMPILER_SHIPPED || '',
  cargoHome: process.env.CARGO_HOME || path.join(os.homedir(), '.cargo'),
  rustupHome: process.env.RUSTUP_HOME || path.join(os.homedir(), '.rustup'),
};

const log = (line) => console.log(`[compiler] ${line}`);

async function builders() {
  const out = [];
  let crates = null;
  for (let i = 0; i < config.slots; i++) {
    const slot = path.join(config.slotsDir, String(i));
    if (await prepareSlot(config.repo, slot)) log(`made slot ${slot} from ${config.repo}`);
    const builder = new Builder({ slot, cargoHome: config.cargoHome, rustupHome: config.rustupHome, timeoutMs: config.timeoutMs });
    const offered = await builder.init(CRATES);
    crates = crates ? crates.filter((c) => offered.includes(c)) : offered;
    out.push(builder);
  }
  return { builders: out, crates };
}

async function prepare() {
  const { builders: all, crates } = await builders();
  const shipped = config.shipped ? JSON.parse(readFileSync(config.shipped, 'utf8')) : null;
  for (const builder of all) {
    for (const crate of crates) {
      const started = Date.now();
      const result = await builder.build(crate, builder.originals.get(crate));
      if (!result.ok) {
        for (const d of result.diagnostics) console.error(d.rendered || d.message);
        throw new Error(`${crate} does not build in ${builder.slot}`);
      }
      log(`${builder.slot}: ${crate} built in ${Date.now() - started} ms, ${result.wasm.length} bytes, sha256 ${result.sha256}`);
      if (shipped && shipped[crate]?.wasm !== result.sha256) {
        throw new Error(`${crate}: the example's own source does not give the module the playground ships (${shipped[crate]?.wasm ?? 'none'} in ` +
          `${config.shipped}): the source and builds.json disagree (node playground/scripts/build-examples.mjs), or this toolchain builds otherwise`);
      }
    }
  }
  if (shipped) log(`every example's own source gives the module the playground ships (${config.shipped})`);
}

async function serve() {
  const { builders: all, crates } = await builders();
  if (crates.length === 0) throw new Error(`no example crate found in ${config.slotsDir}`);
  const rustc = await all[0].rustc();
  if (!rustc) throw new Error('rustc does not run in the slot (is the toolchain of rust-toolchain.toml installed?)');
  const pool = new Pool(all, config.queue);
  const limiter = new Limiter({ perMinute: config.perMinute, burst: Math.min(10, Math.max(1, config.perMinute)) });
  const handler = createHandler({
    pool, limiter, crates, trustProxy: config.trustProxy, cors: config.cors, log,
    info: () => ({ rustc, crates, slots: all.length }),
  });
  const server = http.createServer(handler);
  // a build that waits in the queue and then runs may take minutes; the proxy in front sets its own limits
  server.requestTimeout = config.timeoutMs * (config.queue + 2);
  server.headersTimeout = 30_000;
  setInterval(() => limiter.sweep(), 60_000).unref();
  server.listen(config.port, config.host, () => log(`listening on ${config.host}:${config.port}: ${rustc}, ${all.length} slot(s), ${crates.join(', ')}`));
  const stop = () => {
    log('stopping');
    server.close();
    // builds in progress end with their requests' connections
    server.closeAllConnections();
    setTimeout(() => process.exit(0), 1_000).unref();
  };
  process.on('SIGTERM', stop);
  process.on('SIGINT', stop);
}

const run = process.argv.includes('--prepare') ? prepare : serve;
run().catch((e) => {
  console.error(`[compiler] ${e instanceof Error ? e.message : String(e)}`);
  process.exit(1);
});
