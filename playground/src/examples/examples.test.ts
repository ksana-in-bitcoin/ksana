// The examples the playground ships: each module is the build of the source next to it (scripts/build-examples.mjs), each
// source declares an ABI block the page can read, and the block names the methods and events the contract really has.
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { parseAbi } from '../abi/parse';
import { validateWasm } from '../chain/ops';
import CRATES from '../../compiler/crates.json';
import builds from './wasm/builds.json';

const sha = (bytes: Uint8Array) => createHash('sha256').update(bytes).digest('hex');
const source = (crate: string) => readFileSync(new URL(`../../../contracts/examples/${crate}/src/lib.rs`, import.meta.url));
const wasm = (crate: string) => new Uint8Array(readFileSync(new URL(`./wasm/${crate}.wasm`, import.meta.url)));
const known = builds as Record<string, { source: string; wasm: string; bytes: number } | undefined>;
const sdk = (file: string) => readFileSync(new URL(`../../../crates/ksana-contract-sdk/src/${file}`, import.meta.url), 'utf8');

/**
 * The methods and events of the contract SDK's reference implementation of a standard (`ks20.rs`, `ks721.rs`), which a source
 * built on it answers through its `dispatch(method, args)` and emits through it: each method with the extension that adds it
 * (`"burn" if self.burnable =>`), the three named by constants in `standard.rs` with none.
 */
function standardOf(file: string): { methods: { name: string; extension: string | null }[]; events: string[] } {
  const text = sdk(file);
  const constants = new Map([...sdk('standard.rs').matchAll(/pub const (\w+): &str = "([a-z_]+)";/g)].map((m) => [m[1]!, m[2]!]));
  const start = text.indexOf('pub fn dispatch(&self, method: &str, args: &[u8]) -> Option<CallResult> {');
  const body = text.slice(start, text.indexOf('_ => return None', start));
  const methods = [...body.matchAll(/^\s*(?:"([a-z_]+)"|([A-Z_]+))(?:\s+if self\.(\w+))?\s*=>/gm)]
    .map((m) => ({ name: m[1] ?? constants.get(m[2]!) ?? `?${m[2]}`, extension: m[3] ?? null }));
  const events = [...text.matchAll(/=> encode\(&\("([a-z_]+)"/g)].map((m) => m[1]!);
  return { methods, events };
}

/** The standard methods and events a source answers and emits through the SDK, with the extensions it turns on (`.burnable()`) */
function inherited(text: string): { methods: string[]; events: string[] } {
  const methods: string[] = [];
  const events: string[] = [];
  for (const [type, file] of [['Ks20', 'ks20.rs'], ['Ks721', 'ks721.rs']] as const) {
    if (!new RegExp(`\\b${type}::new\\(\\)`).test(text) || !/\.dispatch\(method, args\)/.test(text)) continue;
    const standard = standardOf(file);
    methods.push(...standard.methods.filter((m) => m.extension === null || text.includes(`.${m.extension}()`)).map((m) => m.name));
    events.push(...standard.events);
  }
  return { methods, events };
}

it('reads the methods and events of the SDK’s reference implementations', () => {
  const ks20 = standardOf('ks20.rs');
  const ks721 = standardOf('ks721.rs');
  expect(ks20.methods.map((m) => m.name)).toEqual(expect.arrayContaining(['standards', 'metadata', 'capabilities', 'transfer', 'approve', 'transfer_and_call']));
  expect(ks20.methods).toContainEqual({ name: 'mint', extension: 'mintable' });
  expect(ks20.events).toEqual(expect.arrayContaining(['standards', 'metadata', 'transfer', 'approve']));
  expect(ks721.methods.map((m) => m.name)).toEqual(expect.arrayContaining(['standards', 'owner_of', 'transfer_from', 'set_operator', 'transfer_and_call']));
  expect(ks721.methods).toContainEqual({ name: 'tokens_of', extension: 'enumerable' });
  expect(ks721.events).toEqual(expect.arrayContaining(['transfer', 'approve', 'operator']));
  for (const m of [...ks20.methods, ...ks721.methods]) expect(m.name).not.toMatch(/^\?/);
});

it('names the same crates as the examples and the compile service', () => {
  const named = [...readFileSync(new URL('./index.ts', import.meta.url), 'utf8').matchAll(/crate: '([a-z-]+)'/g)].map((m) => m[1]);
  expect(named).toEqual(CRATES);
  expect(Object.keys(builds).sort()).toEqual([...CRATES].sort());
});

describe.each(CRATES)('%s', (crate) => {
  it('ships the module built from the current source', async () => {
    const build = known[crate];
    expect(build, `run node scripts/build-examples.mjs ${crate}`).toBeDefined();
    expect(sha(source(crate)), `the source changed: run node scripts/build-examples.mjs ${crate}`).toBe(build!.source);
    const bytes = wasm(crate);
    expect(sha(bytes)).toBe(build!.wasm);
    expect(bytes.length).toBe(build!.bytes);
    expect(bytes.length).toBeLessThan(60 * 1024);
    await validateWasm(bytes);
  });

  it('declares an ABI block that matches its dispatch and events', () => {
    const text = source(crate).toString('utf8');
    const abi = parseAbi(text);
    expect(abi).not.toBeNull();
    const standard = inherited(text);
    // every declared method is a string the dispatch matches on (`"name" =>`, or a tuple arm like `("name", ...)`), or a
    // standard's method the SDK answers for it; every declared event one it emits, or one the SDK emits for it
    for (const m of abi!.methods) {
      if (standard.methods.includes(m.name)) continue;
      expect(text, `${crate}: method ${m.name}`).toMatch(new RegExp(`"${m.name}"\\s*(=>|,|\\|)`));
    }
    for (const e of abi!.events) {
      if (standard.events.includes(e.name)) continue;
      expect(text, `${crate}: event ${e.name}`).toMatch(new RegExp(`emit\\(&\\(\\s*"${e.name}"`));
    }
    const arms = [...text.matchAll(/^\s*"([a-z_]+)" =>/gm)].map((m) => m[1]);
    const declared = new Set([...abi!.methods.map((m) => m.name), ...(abi!.init ? ['init'] : [])]);
    for (const arm of [...arms, ...standard.methods]) expect(declared, `${crate}: ${arm} answers but the ABI block leaves it out`).toContain(arm);
  });
});
