// The contract reference for AI assistants (D-124) as the playground knows it: the pages and crates the compile service builds,
// its limits, the faucet, the ABI block's grammar, and what the Code tab copies. Its SDK signatures, examples, gas and limits are
// held to the code by the Rust tests (crates/ksana-contract-sdk/tests/reference.rs, crates/ksana-execution/tests/reference.rs).
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { parseAbi } from '../abi/parse';
import { parseRoute } from '../app/route';
import { MAX_SOURCE_BYTES } from '../compile/client';
import en from '../i18n/en';
import zhCN from '../i18n/zh-CN';
import CRATES from '../../compiler/crates.json';
import { MAX_CONTEXT_DIAGNOSTICS, REFERENCE, STANDARDS_CRATES, codeContext, referenceSections, type CodeContext } from './reference';

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');
const flat = (text: string) => text.replace(/\s+/g, ' ');
const FLAT = flat(REFERENCE);

/** The text of a `## ` section of the reference, up to the next one */
function section(title: string): string {
  const start = REFERENCE.indexOf(`\n## ${title}\n`);
  expect(start, title).toBeGreaterThan(0);
  const end = REFERENCE.indexOf('\n## ', start + 1);
  return REFERENCE.slice(start, end < 0 ? undefined : end);
}

/** The fenced blocks of the reference marked `lang` */
function blocks(lang: string): string[] {
  return [...REFERENCE.matchAll(new RegExp(`^\`\`\`${lang}\\n([\\s\\S]*?)^\`\`\`$`, 'gm'))].map((m) => m[1]!);
}

/** "a, b or c" */
const list = (items: string[]) => (items.length < 2 ? items.join('') : `${items.slice(0, -1).join(', ')} or ${items.at(-1)}`);

/** The examples as src/examples/index.ts registers them, in its order */
const EXAMPLES = [...read('../examples/index.ts').matchAll(/\{\s*id: '([a-z0-9]+)', crate: '([a-z-]+)'/g)].map((m) => ({
  id: m[1] as keyof typeof en.core.examples,
  crate: m[2]!,
}));

/** Whether an example crate's manifest turns on the contract SDK's `standards` feature */
const hasStandards = (crate: string) =>
  /^ksana-contract-sdk = \{[^}]*features = \[[^\]]*"standards"/m.test(read(`../../../contracts/examples/${crate}/Cargo.toml`));

describe('the playground section', () => {
  it('lists every example page with its crate and features', () => {
    expect(EXAMPLES.map((e) => e.crate)).toEqual(CRATES);
    const rows = section('The playground').split('\n').filter((l) => /^\| [^-]/.test(l)).slice(1);
    expect(rows).toEqual(EXAMPLES.map((e) =>
      `| ${en.core.examples[e.id].title}, \`#/${e.id}/code\` | \`${e.crate}\` | ${hasStandards(e.crate) ? '`standards`' : 'none'} |`));
    for (const e of EXAMPLES) {
      expect(parseRoute(`#/${e.id}/code`)).toEqual({ example: e.id, tab: 'code', contract: null });
    }
  });

  it('knows which crates have the standards', () => {
    expect([...STANDARDS_CRATES].sort()).toEqual(CRATES.filter(hasStandards).sort());
  });

  it('names the pages a contract compiles on as the playground titles them', () => {
    const standards = EXAMPLES.filter((e) => STANDARDS_CRATES.has(e.crate)).map((e) => en.core.examples[e.id].title);
    const plain = en.core.examples.counter.title;
    expect(FLAT).toContain(`compiles on the ${list(standards)} page, any other on the ${plain} page.`);
    expect(FLAT).toContain(`name the page that builds it: ${plain} for a contract on the plain SDK, or ${list(standards)} for one that uses`);
  });

  it('gives the compile service’s limits and the faucet’s amount', () => {
    const build = read('../../compiler/lib/build.mjs');
    const server = read('../../compiler/server.mjs');
    const faucet = read('../../../signet/services/faucet.mjs');
    expect(build).toContain(`export const MAX_SOURCE_BYTES = ${MAX_SOURCE_BYTES / 1024} * 1024;`);
    const timeout = /number\('KSANA_COMPILER_TIMEOUT_SECS', (\d+)/.exec(server)?.[1];
    const perMinute = /number\('KSANA_COMPILER_PER_MINUTE', (\d+)\)/.exec(server)?.[1];
    const ksa = /env\('KSANA_FAUCET_KSA', '(\d+)'\)/.exec(faucet)?.[1];
    const hours = /env\('KSANA_FAUCET_COOLDOWN_HOURS', '(\d+)'\)/.exec(faucet)?.[1];
    expect([timeout, perMinute, ksa, hours].every(Boolean)).toBe(true);
    expect(hours).toBe('24');
    expect(FLAT).toContain(`A source is at most ${MAX_SOURCE_BYTES / 1024} KiB, a build stops after ${timeout} seconds, and one address gets ${perMinute} builds a minute.`);
    expect(FLAT).toContain(`The faucet sends ${ksa} test KSA a day to each account.`);
  });
});

describe('the ABI block', () => {
  it('lists the types the parser takes', () => {
    const parser = read('../abi/parse.ts');
    const set = (name: string) => [...(new RegExp(`const ${name} = new Set<string>\\(\\[([^\\]]*)\\]`).exec(parser)?.[1] ?? '').matchAll(/'(\w+)'/g)].map((m) => m[1]!);
    const named = [...set('INTS'), ...set('IDS'), 'bool', 'String', 'Bytes'];
    expect(named.length).toBeGreaterThan(15);
    const types = flat(section('The ABI block')).split('- Types: ')[1]!.split(' and the structs and enums')[0]!;
    const listed = [...types.matchAll(/`(\w+)`/g)].map((m) => m[1]!);
    expect(listed.sort()).toEqual(named.sort());
    for (const type of [...named, '[u8; 20]', 'Option<u64>', 'Vec<Account>', '(String, u64)']) {
      expect(parseAbi(`//! \`\`\`abi\n//! view f(a: ${type}) -> ${type}\n//! \`\`\`\n`), type).not.toBeNull();
    }
  });

  it('parses the reference’s example block as it stands', () => {
    const example = blocks('text').find((b) => b.startsWith('//! ```abi'));
    expect(example).toBeDefined();
    const abi = parseAbi(example!);
    expect(abi?.init).not.toBeNull();
    expect(abi?.methods.map((m) => m.name)).toEqual(['deposit', 'balance_of']);
  });

  it('parses the standards’ blocks', () => {
    const abi = blocks('abi');
    expect(abi).toHaveLength(2);
    for (const block of abi) {
      const parsed = parseAbi(['//! ```abi', ...block.trimEnd().split('\n').map((l) => `//! ${l}`), '//! ```', ''].join('\n'));
      expect(parsed?.methods.length).toBeGreaterThan(10);
      expect(parsed?.events.length).toBeGreaterThan(3);
    }
  });

  it('parses the ABI block of every Rust block that has one', () => {
    const rust = blocks('rust').filter((b) => b.includes('```abi'));
    expect(rust.length).toBeGreaterThanOrEqual(2);
    for (const block of rust) expect(parseAbi(block)).not.toBeNull();
  });
});

describe('the reference', () => {
  it('lists its sections outside its code blocks', () => {
    const sections = referenceSections();
    expect(sections[0]).toBe('Rules for the assistant');
    expect(sections).toContain('The playground');
    expect(sections).toContain('Mistakes to avoid');
    expect(new Set(sections).size).toBe(sections.length);
    expect(sections.length).toBe([...REFERENCE.matchAll(/^## /gm)].length);
  });

  it('is linked from both languages’ pages', () => {
    expect(en.core.ai.steps.copy.body).toContain('ksana.network/llms-contracts.txt');
    expect(zhCN.core.ai.steps.copy.body).toContain('ksana.network/llms-contracts.txt');
    expect(REFERENCE).toContain('https://ksana.network/llms-contracts.txt');
    expect(zhCN.core.ai.prompts).toHaveLength(en.core.ai.prompts.length);
  });
});

describe('what the Code tab copies', () => {
  const counter = read('../../../contracts/examples/playground-counter/src/lib.rs');
  const base: CodeContext = {
    page: 'Counter', route: '#/counter/code', crate: 'playground-counter', source: counter, edited: false, diagnostics: [], locale: 'en', withReference: true,
  };
  const error = { level: 'error', message: 'cannot find value `x`', rendered: 'error[E0425]: cannot find value `x` in this scope\n --> src/lib.rs:3:5\n' };
  const warning = { level: 'warning', message: 'unused variable: `y`', rendered: 'warning: unused variable: `y`\n' };

  /** The source between the fences of the first `rust` block after the contract's heading */
  const sourceIn = (text: string) => {
    const mine = text.slice(text.lastIndexOf('# My contract'));
    const m = /^(`{3,})rust\n([\s\S]*?)\n\1$/m.exec(mine);
    return m?.[2] ?? null;
  };

  it('puts the reference first, or leaves it out', () => {
    const full = codeContext(base);
    expect(full.startsWith(REFERENCE.trimEnd())).toBe(true);
    const short = codeContext({ ...base, withReference: false });
    expect(short.startsWith('# My contract\n')).toBe(true);
    expect(short).not.toContain('## Rules for the assistant');
    expect(short).toContain('reference I gave you earlier');
  });

  it('keeps every example’s source whole inside its fence', () => {
    for (const crate of CRATES) {
      const source = read(`../../../contracts/examples/${crate}/src/lib.rs`);
      const text = codeContext({ ...base, crate, source, withReference: false });
      expect(sourceIn(text), crate).toBe(source.trimEnd());
      expect(text).toContain(STANDARDS_CRATES.has(crate) ? 'has the contract SDK\'s `standards` feature' : 'on the plain contract SDK');
    }
    const fenced = 'fn f() {}\n// ````` a long fence\n';
    expect(sourceIn(codeContext({ ...base, source: fenced }))).toBe(fenced.trimEnd());
  });

  it('asks for fixes when the compiler found errors', () => {
    expect(codeContext(base)).not.toContain('## What the compiler said');
    const withError = codeContext({ ...base, edited: true, diagnostics: [warning, error] });
    expect(withError).toContain('## What the compiler said');
    expect(withError).toContain(error.rendered.trimEnd());
    expect(withError).toContain('fix the compiler\'s errors');
    expect(withError).toContain('It is my edit of the example.');
    const withWarning = codeContext({ ...base, diagnostics: [warning] });
    expect(withWarning).toContain(warning.rendered.trimEnd());
    expect(withWarning).toContain('do what I ask below');
  });

  it('copies at most a screenful of diagnostics', () => {
    const many = Array.from({ length: MAX_CONTEXT_DIAGNOSTICS + 5 }, (_, i) => ({ ...error, rendered: `error: number ${i}\n` }));
    const text = codeContext({ ...base, diagnostics: many });
    expect(text).toContain(`error: number ${MAX_CONTEXT_DIAGNOSTICS - 1}`);
    expect(text).not.toContain(`error: number ${MAX_CONTEXT_DIAGNOSTICS}\n`);
    expect(text).toContain('(5 more not shown)');
  });

  it('asks for answers in the page’s language', () => {
    expect(codeContext(base)).not.toContain('Simplified Chinese');
    expect(codeContext({ ...base, locale: 'zh-CN' }).trimEnd().endsWith('Answer in Simplified Chinese; keep code, identifiers and revert reasons in English.')).toBe(true);
  });
});
