// Builds the playground's example contracts as the compile service builds them, in a slot of its own
// (contracts/target/playground-slot; compiler/lib/build.mjs): the repository's pinned toolchain, the contracts workspace's
// linker flags, the paths remapped and the SDK reached from inside the workspace's root, so the modules do not depend on
// where the repository is checked out. Writes each module to src/examples/wasm/<crate>.wasm with builds.json (the SHA-256 of
// each source and module). The crates of the contracts' lockfile are fetched first when they are missing; no node or wallet is
// contacted. Run it after changing an example's source: src/examples/examples.test.ts fails while a source differs from the
// one its module was built from.
//
//   node scripts/build-examples.mjs [<crate> ...]      (from playground/; all examples by default)
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Builder, prepareSlot } from '../compiler/lib/build.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const out = path.join(root, 'playground/src/examples/wasm');
const manifest = path.join(out, 'builds.json');
const slot = path.join(root, 'contracts/target/playground-slot');

/** The examples the playground ships, by crate (src/examples/index.ts names the same crates, the compile service builds them) */
const CRATES = JSON.parse(readFileSync(path.join(root, 'playground/compiler/crates.json'), 'utf8'));

const asked = process.argv.slice(2);
for (const crate of asked) if (!CRATES.includes(crate)) throw new Error(`not an example of the playground: ${crate}`);

// a slot builds offline
execFileSync('cargo', ['fetch', '--locked', '--manifest-path', path.join(root, 'contracts/Cargo.toml')], { cwd: root, stdio: 'inherit' });
if (await prepareSlot(root, slot)) console.log(`made the slot ${slot}`);
const builder = new Builder({
  slot,
  cargoHome: process.env.CARGO_HOME || path.join(homedir(), '.cargo'),
  rustupHome: process.env.RUSTUP_HOME || path.join(homedir(), '.rustup'),
  timeoutMs: 30 * 60_000,
});
await builder.init(CRATES);

const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
const builds = existsSync(manifest) ? JSON.parse(readFileSync(manifest, 'utf8')) : {};
mkdirSync(out, { recursive: true });

for (const crate of asked.length ? asked : CRATES) {
  const source = readFileSync(path.join(root, 'contracts/examples', crate, 'src/lib.rs'));
  const result = await builder.build(crate, source);
  if (!result.ok) {
    for (const d of result.diagnostics) console.error(d.rendered || d.message);
    throw new Error(`${crate} does not build`);
  }
  writeFileSync(path.join(out, `${crate}.wasm`), result.wasm);
  builds[crate] = { source: hash(source), wasm: result.sha256, bytes: result.wasm.length };
  console.log(`${crate}: ${result.wasm.length} bytes`);
}

const sorted = Object.fromEntries(CRATES.filter((c) => builds[c]).map((c) => [c, builds[c]]));
writeFileSync(manifest, `${JSON.stringify(sorted, null, 2)}\n`);
