// The compile service's builds: rustc's messages as the page reads them, the slots, and, where cargo and the wasm32 target
// are installed, real builds in a slot made from this checkout (KSANA_COMPILER_SKIP_CARGO=1 skips those).
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { appendFileSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';
import { Builder, diagnosticsOf, prepareSlot } from '../lib/build.mjs';

const repo = fileURLToPath(new URL('../../../', import.meta.url));
const CRATES = JSON.parse(readFileSync(path.join(repo, 'playground/compiler/crates.json'), 'utf8'));
const slot = '/srv/slots/0';
const cargoHome = '/usr/local/cargo';

const message = (crate, diagnostic) => ({
  reason: 'compiler-message',
  manifest_path: `${slot}/contracts/examples/${crate}/Cargo.toml`,
  message: diagnostic,
});
const span = (file, line, primary = true) => ({ file_name: file, line_start: line, line_end: line, column_start: 5, column_end: 9, is_primary: primary, label: 'here' });

describe("rustc's messages", () => {
  it("keeps the crate's errors and warnings with their spans in the source, as src/lib.rs", () => {
    const out = diagnosticsOf([
      message('amm', {
        level: 'error', message: 'mismatched types',
        rendered: `error[E0308]: mismatched types\n  --> examples/amm/src/lib.rs:12:5\n   |\nnote: in ${cargoHome}/registry/src/x/borsh/src/lib.rs and ${slot}/crates/ksana-contract-sdk/src/api.rs\n`,
        spans: [span('examples/amm/src/lib.rs', 12), span(`${cargoHome}/registry/src/x/borsh/src/lib.rs`, 3, false)],
      }),
      message('amm', { level: 'warning', message: 'unused variable: `x`', rendered: 'warning: unused variable\n', spans: [span(`${slot}/contracts/examples/amm/src/lib.rs`, 40)] }),
      message('amm', { level: 'error', message: 'aborting due to 1 previous error', rendered: '', spans: [] }),
      message('amm', { level: 'warning', message: '1 warning emitted', rendered: '', spans: [] }),
      message('amm', { level: 'note', message: 'a note', rendered: '', spans: [] }),
      message('ksana-contract-sdk', { level: 'warning', message: 'in a dependency', rendered: '', spans: [] }),
      { reason: 'compiler-artifact', manifest_path: `${slot}/contracts/examples/amm/Cargo.toml` },
    ], { crate: 'amm', slot, cargoHome });
    assert.equal(out.length, 2);
    assert.deepEqual(out[0].spans, [{ line_start: 12, line_end: 12, column_start: 5, column_end: 9, is_primary: true, label: 'here' }]);
    assert.equal(out[0].level, 'error');
    assert.match(out[0].rendered, /--> src\/lib\.rs:12:5/);
    assert.match(out[0].rendered, /in \/cargo\/registry\/src\/x\/borsh\/src\/lib\.rs and \/ksana\/crates\/ksana-contract-sdk\/src\/api\.rs/);
    assert.ok(!out[0].rendered.includes(slot));
    assert.equal(out[1].level, 'warning');
    assert.equal(out[1].spans[0].line_start, 40);
  });

  it('takes an internal compiler error as an error', () => {
    const out = diagnosticsOf([message('nft', { level: 'error: internal compiler error', message: 'boom', rendered: 'boom', spans: [] })], { crate: 'nft', slot, cargoHome });
    assert.equal(out[0].level, 'error');
  });
});

describe('a slot', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'ksana-compiler-slot-'));
  after(() => rmSync(dir, { recursive: true, force: true }));
  const at = path.join(dir, '0');

  it('is made once, reaches the SDK from inside the contracts workspace, and is made again when its files differ', async () => {
    assert.equal(await prepareSlot(repo, at), true);
    assert.equal(readlinkSync(path.join(at, 'contracts/sdk')), '../crates/ksana-contract-sdk');
    const manifest = readFileSync(path.join(at, 'contracts/Cargo.toml'), 'utf8');
    assert.match(manifest, /^ksana-contract-sdk = \{ path = "sdk" \}$/m);
    assert.match(manifest, /^exclude = \["sdk"\]$/m);
    assert.equal(await prepareSlot(repo, at), false);
    appendFileSync(path.join(at, 'contracts/examples/amm/src/lib.rs'), '\n// left by a stopped build\n');
    assert.equal(await prepareSlot(repo, at), true);
    assert.deepEqual(readFileSync(path.join(at, 'contracts/examples/amm/src/lib.rs')), readFileSync(path.join(repo, 'contracts/examples/amm/src/lib.rs')));
    assert.equal(await prepareSlot(repo, at), false);
  });
});

/**
 * rustc as a machine of another architecture answers: its own host, and a standard library for no machine but itself and
 * wasm32, as an aarch64 one with the wasm32 target does
 */
const foreignRustc = (real, host) => `#!/bin/sh
real='${real}'
# macOS drops DYLD_ variables when it starts /bin/sh: the toolchain's lib directory again for its linker (lib/rustc.sh)
if [ "$(uname -s)" = Darwin ]; then
  DYLD_FALLBACK_LIBRARY_PATH="\${real%/*}/../lib:\${DYLD_FALLBACK_LIBRARY_PATH:-$HOME/lib:/usr/local/lib:/usr/lib}"
  export DYLD_FALLBACK_LIBRARY_PATH
fi
own=$("$real" -vV | sed -n 's/^host: //p')
if [ "$#" -eq 1 ] && [ "$1" = -vV ]; then
  "$real" -vV | sed 's/^host: .*/host: ${host}/'
  exit
fi
previous=
for arg do
  shift
  case $previous/$arg in
  --target/${host}) arg=$own ;;
  */--target=${host}) arg=--target=$own ;;
  --target/wasm32-unknown-unknown | */--target=wasm32-unknown-unknown) ;;
  --target/* | */--target=*) exit 1 ;;
  esac
  previous=$arg
  set -- "$@" "$arg"
done
exec "$real" "$@"
`;

function cargoReady() {
  if (process.env.KSANA_COMPILER_SKIP_CARGO === '1') return false;
  try {
    const targets = execFileSync('rustup', ['target', 'list', '--installed'], { cwd: repo, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
    return targets.split('\n').includes('wasm32-unknown-unknown');
  } catch {
    return false;
  }
}

describe('a build in a slot', { skip: cargoReady() ? false : 'cargo with the wasm32-unknown-unknown target is not installed' }, () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'ksana-compiler-test-'));
  after(() => rmSync(dir, { recursive: true, force: true }));
  const shipped = JSON.parse(readFileSync(path.join(repo, 'playground/src/examples/wasm/builds.json'), 'utf8'));
  let builder;

  // in a slot elsewhere than this checkout: the modules depend on neither path (contracts/Cargo.toml)
  it("gives the modules the playground ships for the examples' own sources", { timeout: 1_800_000 }, async () => {
    await prepareSlot(repo, path.join(dir, '0'));
    builder = new Builder({
      slot: path.join(dir, '0'),
      cargoHome: process.env.CARGO_HOME || path.join(os.homedir(), '.cargo'),
      rustupHome: process.env.RUSTUP_HOME || path.join(os.homedir(), '.rustup'),
      timeoutMs: 600_000,
    });
    assert.deepEqual(await builder.init(CRATES), CRATES);
    for (const crate of CRATES) {
      const result = await builder.build(crate, builder.originals.get(crate));
      assert.equal(result.ok, true, `${crate}: ${JSON.stringify(result.diagnostics)}`);
      assert.equal(result.sha256, shipped[crate].wasm, crate);
      assert.equal(result.wasm.length, shipped[crate].bytes, crate);
    }
  });

  it("answers an error with its line in the source, and puts the example's source back", { timeout: 600_000 }, async () => {
    const source = builder.originals.get('playground-counter').toString('utf8');
    const lines = source.split('\n');
    const at = lines.findIndex((l) => l.startsWith('fn change('));
    assert.ok(at >= 0);
    lines.splice(at, 0, 'fn broken() -> u64 { "text" }');
    const result = await builder.build('playground-counter', lines.join('\n'));
    assert.equal(result.ok, false);
    const error = result.diagnostics.find((d) => d.level === 'error');
    assert.ok(error, JSON.stringify(result.diagnostics));
    assert.equal(error.spans.find((s) => s.is_primary)?.line_start, at + 1);
    assert.match(error.rendered, /--> src\/lib\.rs:/);
    assert.deepEqual(readFileSync(builder.sourcePath('playground-counter')), builder.originals.get('playground-counter'));
  });

  // cargo hashes the host rustc names into the metadata of build scripts and proc macros, and through them into the
  // crates for wasm32: amm's and multisig's functions came out in another order on aarch64 Linux (lib/rustc.sh)
  it('gives the same modules on a machine of another architecture', { timeout: 1_800_000 }, async () => {
    const sysroot = execFileSync('rustc', ['--print', 'sysroot'], { cwd: repo, encoding: 'utf8' }).trim();
    const rustc = path.join(dir, 'rustc');
    writeFileSync(rustc, foreignRustc(path.join(sysroot, 'bin', 'rustc'), 'riscv64gc-unknown-linux-gnu'), { mode: 0o755 });
    await prepareSlot(repo, path.join(dir, '1'));
    const foreign = new Builder({
      slot: path.join(dir, '1'),
      rustc,
      cargoHome: process.env.CARGO_HOME || path.join(os.homedir(), '.cargo'),
      rustupHome: process.env.RUSTUP_HOME || path.join(os.homedir(), '.rustup'),
      timeoutMs: 600_000,
    });
    await foreign.init(CRATES);
    for (const crate of ['amm', 'multisig']) {
      const result = await foreign.build(crate, foreign.originals.get(crate));
      assert.equal(result.ok, true, `${crate}: ${JSON.stringify(result.diagnostics)}`);
      assert.equal(result.sha256, shipped[crate].wasm, crate);
    }
    // proc-macro2's build script probes the compiler with --target set to the host cargo knows; it got the machine's own
    const built = path.join(dir, '1', 'target', 'release', 'build');
    const probed = readdirSync(built)
      .filter((d) => d.startsWith('proc-macro2-'))
      .some((d) => {
        try {
          return readFileSync(path.join(built, d, 'output'), 'utf8').includes('proc_macro_span_location');
        } catch {
          return false;
        }
      });
    assert.ok(probed, "proc-macro2's probe failed");
  });

  it("keeps the service's environment out of the build", { timeout: 600_000 }, async () => {
    process.env.KSANA_COMPILER_TEST_SECRET = 'not for builds';
    try {
      const source = `${builder.originals.get('playground-counter').toString('utf8')}\nconst _: () = assert!(option_env!("KSANA_COMPILER_TEST_SECRET").is_none());\n`;
      const result = await builder.build('playground-counter', source);
      assert.equal(result.ok, true, JSON.stringify(result.diagnostics));
    } finally {
      delete process.env.KSANA_COMPILER_TEST_SECRET;
    }
  });
});
