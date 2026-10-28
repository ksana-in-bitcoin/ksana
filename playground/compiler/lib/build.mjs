// The builds of the compile service: a contract's source compiled as one of the playground's example crates, in a copy
// of the contracts workspace (a slot) that holds only what such a build reads: the root manifest and rust-toolchain.toml
// (the SDK inherits its workspace's fields), the SDK's manifest and sources, and contracts/ with its lockfile and linker
// flags, the SDK reached from inside the workspace's root (placeSdk). Nothing in a build depends on where its slot is or on
// the machine's architecture (rustc.sh), and scripts/build-examples.mjs builds the shipped modules in a slot of its own,
// so an example's own source gives the module the playground ships, byte for byte.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { cp, lstat, mkdir, readdir, readFile, readlink, rename, rm, stat, symlink, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** The most source a build takes, as the page's client says (playground/src/compile/client.ts) */
export const MAX_SOURCE_BYTES = 128 * 1024;
/** A module larger than this is not sent back (a deployment holds 64 KiB with the rest of its operation) */
export const MAX_WASM_BYTES = 1024 * 1024;
const MAX_STDOUT_BYTES = 8 * 1024 * 1024;
const MAX_STDERR_BYTES = 64 * 1024;
const MAX_DIAGNOSTICS = 200;
const MAX_RENDERED = 16 * 1024;

/** The rustc cargo runs: the toolchain's, with one host line on every machine (see the script) */
const RUSTC = fileURLToPath(new URL('./rustc.sh', import.meta.url));

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

async function exists(file) {
  try {
    await stat(file);
    return true;
  } catch {
    return false;
  }
}

/** The files of the repository a slot is made of, relative to its root (directories copied whole) */
async function slotFiles(repo) {
  const files = ['Cargo.toml', 'rust-toolchain.toml', 'crates/ksana-contract-sdk/Cargo.toml', 'crates/ksana-contract-sdk/src',
    'contracts/Cargo.toml', 'contracts/Cargo.lock', 'contracts/.cargo/config.toml'];
  // every member of the contracts workspace must be there for cargo to load it; of each, the manifest and the sources
  for (const group of ['examples', 'testing']) {
    for (const entry of await readdir(path.join(repo, 'contracts', group), { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const crate = `contracts/${group}/${entry.name}`;
      if (!(await exists(path.join(repo, crate, 'Cargo.toml')))) continue;
      files.push(`${crate}/Cargo.toml`, `${crate}/src`);
    }
  }
  return files;
}

/** A digest of `files` (as slotFiles lists them) under `base`: every path, link and content */
async function digestOf(base, files) {
  const hash = createHash('sha256');
  const visit = async (rel) => {
    const file = path.join(base, rel);
    const info = await lstat(file).catch(() => null);
    if (!info) hash.update(`missing ${rel}\n`);
    else if (info.isSymbolicLink()) hash.update(`link ${rel} ${await readlink(file)}\n`);
    else if (info.isDirectory()) for (const entry of (await readdir(file)).sort()) await visit(path.join(rel, entry));
    else hash.update(`file ${rel} ${info.size}\n`).update(await readFile(file));
  };
  for (const rel of files) await visit(rel);
  return hash.digest('hex');
}

/**
 * Puts the SDK inside the contracts workspace's root in a slot. Cargo hashes a path dependency outside the workspace's root
 * by its absolute path, and the symbol names, and with them the order of a module's functions, follow that hash (D-031): the
 * modules would depend on where the slot is. Reached through contracts/sdk, a link the workspace excludes, the SDK is hashed
 * by its path relative to the root, and a module is the same in every slot. (The repository keeps the SDK where it is:
 * through such a link in the checkout, `cargo fmt --all` would take the SDK for a package of no workspace.)
 */
async function placeSdk(slot) {
  const manifest = path.join(slot, 'contracts', 'Cargo.toml');
  const text = await readFile(manifest, 'utf8');
  const dependency = 'ksana-contract-sdk = { path = "../crates/ksana-contract-sdk" }';
  const members = /^members = \[.*\]$/m;
  if (text.split(dependency).length !== 2 || !members.test(text)) {
    throw new Error('contracts/Cargo.toml no longer names the SDK as playground/compiler/lib/build.mjs expects (placeSdk)');
  }
  await writeFile(manifest, text.replace(dependency, 'ksana-contract-sdk = { path = "sdk" }').replace(members, (line) => `${line}\nexclude = ["sdk"]`));
  await symlink('../crates/ksana-contract-sdk', path.join(slot, 'contracts', 'sdk'));
}

/** What a slot records of how it was made: the digests of the repository's files it was made from and of its own */
const MARK = '.ksana-slot.json';

/**
 * Makes a slot at `dir` from a checkout (or the image's copy) of the repository, and returns whether it made one. A slot
 * already there is kept while it holds what it was made with from the repository's files as they are now (a build puts
 * the example's source back), and made again otherwise, after a change to the contracts workspace or the SDK for instance.
 */
export async function prepareSlot(repo, dir) {
  const files = await slotFiles(repo);
  const own = [...files, 'contracts/sdk'];
  const made = { repo: await digestOf(repo, files), slot: '' };
  const mark = await readFile(path.join(dir, MARK), 'utf8').then((text) => JSON.parse(text), () => null);
  if (mark?.repo === made.repo && mark.slot === (await digestOf(dir, own))) return false;
  const partial = `${dir}.partial`;
  await rm(partial, { recursive: true, force: true });
  for (const file of files) {
    await mkdir(path.dirname(path.join(partial, file)), { recursive: true });
    // timestamps kept: cargo compares them with its fingerprints
    await cp(path.join(repo, file), path.join(partial, file), { recursive: true, preserveTimestamps: true });
  }
  await placeSdk(partial);
  made.slot = await digestOf(partial, own);
  await writeFile(path.join(partial, MARK), `${JSON.stringify(made)}\n`);
  await rm(dir, { recursive: true, force: true });
  await rename(partial, dir);
  return true;
}

/** The rustflags of contracts/.cargo/config.toml, with the slot's and Cargo's paths remapped in every crate of a build */
export async function rustflagsOf(slot, cargoHome) {
  const config = await readFile(path.join(slot, 'contracts', '.cargo', 'config.toml'), 'utf8');
  const configured = /^rustflags\s*=\s*(\[[^\n]+\])/m.exec(config);
  if (!configured) throw new Error('cannot read the linker flags of contracts/.cargo/config.toml');
  const flags = JSON.parse(configured[1]);
  const root = slot.endsWith(path.sep) ? slot : `${slot}${path.sep}`;
  flags.push(`--remap-path-prefix=${root}=/ksana/`, `--remap-path-prefix=${cargoHome}=/cargo`);
  return flags;
}

/** The user's file as a span or a rendered message names it: relative to contracts/, absolute, or remapped */
function isSourceFile(name, crate, slot) {
  const own = `examples/${crate}/src/lib.rs`;
  return name === own || name === path.join(slot, 'contracts', own) || name === `/ksana/contracts/${own}`;
}

/** Paths in rustc's text as a reader of the playground knows them: the source as src/lib.rs, the rest remapped */
function cleanText(text, crate, slot, cargoHome) {
  const own = `examples/${crate}/src/lib.rs`;
  return text
    .split(path.join(slot, 'contracts', own)).join('src/lib.rs')
    .split(`/ksana/contracts/${own}`).join('src/lib.rs')
    .split(own).join('src/lib.rs')
    .split(`${slot}${path.sep}`).join('/ksana/')
    .split(cargoHome).join('/cargo');
}

/**
 * The compiler's messages about the user's crate (`cargo --message-format=json`), as the page reads them: errors and
 * warnings with their spans in the source; spans elsewhere (the SDK, the standard library) only in the rendered text.
 */
export function diagnosticsOf(messages, { crate, slot, cargoHome }) {
  const out = [];
  for (const m of messages) {
    if (m?.reason !== 'compiler-message' || typeof m.manifest_path !== 'string') continue;
    if (!m.manifest_path.endsWith(`${path.sep}examples${path.sep}${crate}${path.sep}Cargo.toml`)) continue;
    const d = m.message;
    if (!d || typeof d.message !== 'string') continue;
    const level = typeof d.level === 'string' && d.level.startsWith('error') ? 'error' : d.level;
    if (level !== 'error' && level !== 'warning') continue;
    if (/^aborting due to/.test(d.message) || /^\d+ warnings? emitted/.test(d.message)) continue;
    const spans = (Array.isArray(d.spans) ? d.spans : [])
      .filter((s) => typeof s?.file_name === 'string' && isSourceFile(s.file_name, crate, slot))
      .map((s) => ({
        line_start: s.line_start, line_end: s.line_end, column_start: s.column_start, column_end: s.column_end,
        is_primary: s.is_primary === true, label: typeof s.label === 'string' ? s.label : null,
      }));
    const rendered = typeof d.rendered === 'string' ? cleanText(d.rendered, crate, slot, cargoHome).slice(0, MAX_RENDERED) : '';
    out.push({ level, message: cleanText(d.message, crate, slot, cargoHome), rendered, spans });
    if (out.length >= MAX_DIAGNOSTICS) break;
  }
  return out;
}

/** A diagnostic of the service's own, with no position */
export function notice(message) {
  return { level: 'error', message, rendered: `error: ${message}\n`, spans: [] };
}

/** One slot's builds, one at a time */
export class Builder {
  /**
   * @param {{ slot: string, cargo?: string, rustc?: string, cargoHome: string, rustupHome: string, path?: string, timeoutMs: number }} options
   *   `rustc`: the toolchain's rustc, by default the one rustup picks for the slot
   */
  constructor({ slot, cargo = 'cargo', rustc, cargoHome, rustupHome, path: searchPath, timeoutMs }) {
    this.slot = path.resolve(slot);
    this.cargo = cargo;
    this.toolchainRustc = rustc ?? null;
    this.cargoHome = cargoHome;
    this.rustupHome = rustupHome;
    this.searchPath = searchPath ?? process.env.PATH ?? '/usr/local/bin:/usr/bin:/bin';
    this.timeoutMs = timeoutMs;
    this.originals = new Map();
    this.flags = null;
  }

  /** Reads the examples' own sources, put back after every build, and the build flags; returns the crates it can build */
  async init(crates) {
    this.flags = await rustflagsOf(this.slot, this.cargoHome);
    if (!this.toolchainRustc) {
      const { code, stdout } = await this.spawn(['--print', 'sysroot'], { command: 'rustc', timeoutMs: 60_000 });
      if (code !== 0) throw new Error("cannot find the toolchain's rustc (rustc --print sysroot)");
      this.toolchainRustc = path.join(stdout.toString('utf8').trim(), 'bin', 'rustc');
    }
    for (const crate of crates) {
      const file = this.sourcePath(crate);
      if (await exists(file)) this.originals.set(crate, await readFile(file));
    }
    return [...this.originals.keys()];
  }

  sourcePath(crate) {
    return path.join(this.slot, 'contracts', 'examples', crate, 'src', 'lib.rs');
  }

  env() {
    // Only what cargo and rustup need: the service's own environment stays out of the build, as a source can read
    // variables at compile time (env!)
    return {
      PATH: this.searchPath,
      HOME: this.slot,
      CARGO_HOME: this.cargoHome,
      CARGO_TARGET_DIR: path.join(this.slot, 'target'),
      CARGO_ENCODED_RUSTFLAGS: this.flags.join('\x1f'),
      CARGO_NET_OFFLINE: 'true',
      CARGO_TERM_COLOR: 'never',
      CARGO_TERM_PROGRESS_WHEN: 'never',
      RUSTUP_HOME: this.rustupHome,
      ...(this.toolchainRustc ? { RUSTC, KSANA_RUSTC: this.toolchainRustc } : {}),
    };
  }

  /** The toolchain's version, as the slot's rust-toolchain.toml picks it */
  async rustc() {
    const { code, stdout } = await this.spawn(['--version'], { command: 'rustc', timeoutMs: 60_000 });
    return code === 0 ? stdout.toString('utf8').trim() : null;
  }

  spawn(args, { command = this.cargo, timeoutMs = this.timeoutMs, signal } = {}) {
    return new Promise((resolve, reject) => {
      const child = spawn(command, args, {
        cwd: path.join(this.slot, 'contracts'),
        env: this.env(),
        stdio: ['ignore', 'pipe', 'pipe'],
        // its own process group, so that a stopped build takes rustc and the linker with it
        detached: true,
      });
      const out = [];
      const err = [];
      let outBytes = 0;
      let errBytes = 0;
      let stopped = null;
      const stop = (why) => {
        if (stopped) return;
        stopped = why;
        try {
          process.kill(-child.pid, 'SIGKILL');
        } catch {
          child.kill('SIGKILL');
        }
      };
      const timer = setTimeout(() => stop('timeout'), timeoutMs);
      const onAbort = () => stop('aborted');
      signal?.addEventListener('abort', onAbort, { once: true });
      child.stdout.on('data', (chunk) => {
        if (outBytes < MAX_STDOUT_BYTES) out.push(chunk);
        outBytes += chunk.length;
      });
      child.stderr.on('data', (chunk) => {
        if (errBytes < MAX_STDERR_BYTES) err.push(chunk);
        errBytes += chunk.length;
      });
      child.on('error', (e) => {
        clearTimeout(timer);
        signal?.removeEventListener('abort', onAbort);
        reject(e);
      });
      child.on('close', (code) => {
        clearTimeout(timer);
        signal?.removeEventListener('abort', onAbort);
        resolve({ code, stopped, stdout: Buffer.concat(out), stderr: Buffer.concat(err).toString('utf8'), truncated: outBytes > MAX_STDOUT_BYTES });
      });
    });
  }

  /**
   * Builds `source` as `crate`: `{ ok: true, wasm, sha256, diagnostics }` or `{ ok: false, diagnostics }`. The example's
   * own source is put back afterwards, so nothing of a visitor's stays in the slot but the build's output.
   */
  async build(crate, source, signal) {
    const original = this.originals.get(crate);
    if (!original) throw new Error(`not a crate this service builds: ${crate}`);
    const file = this.sourcePath(crate);
    const wasmPath = path.join(this.slot, 'target', 'wasm32-unknown-unknown', 'release', `${crate.replaceAll('-', '_')}.wasm`);
    await rm(wasmPath, { force: true });
    await writeFile(file, source);
    try {
      const run = await this.spawn(['rustc', '--locked', '--offline', '--release', '--target', 'wasm32-unknown-unknown', '-p', crate,
        '--crate-type', 'cdylib', '--message-format=json'], { signal });
      if (run.stopped === 'aborted') return { ok: false, aborted: true, diagnostics: [] };
      if (run.stopped === 'timeout') {
        return { ok: false, diagnostics: [notice(`the build took longer than ${Math.round(this.timeoutMs / 1000)} s and was stopped`)] };
      }
      const messages = [];
      for (const line of run.stdout.toString('utf8').split('\n')) {
        if (!line.startsWith('{')) continue;
        try {
          messages.push(JSON.parse(line));
        } catch {
          /* a line cut by the output limit */
        }
      }
      const diagnostics = diagnosticsOf(messages, { crate, slot: this.slot, cargoHome: this.cargoHome });
      if (run.code !== 0) {
        if (!diagnostics.some((d) => d.level === 'error')) {
          // cargo's own failure (no compiler message): its last lines, with the paths cleaned
          const tail = cleanText(run.stderr, crate, this.slot, this.cargoHome).trim().split('\n').slice(-8).join('\n');
          diagnostics.push(notice(tail || `the build failed (exit status ${run.code})`));
        }
        return { ok: false, diagnostics };
      }
      const info = await stat(wasmPath).catch(() => null);
      if (!info) return { ok: false, diagnostics: [...diagnostics, notice('the build produced no module')] };
      if (info.size > MAX_WASM_BYTES) {
        return { ok: false, diagnostics: [...diagnostics, notice(`the module is ${info.size} bytes, more than a deployment can hold`)] };
      }
      const wasm = await readFile(wasmPath);
      return { ok: true, wasm, sha256: sha256(wasm), diagnostics };
    } finally {
      await writeFile(file, original);
    }
  }
}
