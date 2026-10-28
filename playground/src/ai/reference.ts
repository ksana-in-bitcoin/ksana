/**
 * The contract reference for AI assistants (D-124): `ksana-contracts.md` beside this file, which the site also publishes as
 * https://ksana.network/llms-contracts.txt. The Rust tests hold it to the code (its SDK signatures and examples in
 * crates/ksana-contract-sdk/tests/reference.rs, its gas, limits and parameters in crates/ksana-execution/tests/reference.rs),
 * and reference.test.ts to the playground. The Code tab copies it together with the contract being edited.
 */
import text from './ksana-contracts.md?raw';

/** The reference, in Markdown */
export const REFERENCE: string = text;

/** Where the site publishes it */
export const REFERENCE_URL = 'https://ksana.network/llms-contracts.txt';

/** The name of the file the page downloads */
export const REFERENCE_FILE = 'ksana-contracts.md';

/** The crates of contracts/examples/ that have the contract SDK's `standards` feature, as their manifests say */
export const STANDARDS_CRATES: ReadonlySet<string> = new Set(['token', 'nft', 'amm']);

/** The most diagnostics the Code tab copies; the rest are counted */
export const MAX_CONTEXT_DIAGNOSTICS = 20;

/** The size of the reference, in KiB, as the page shows it */
export function referenceKb(): number {
  return Math.round(new TextEncoder().encode(REFERENCE).length / 1024);
}

/** The reference's sections, as its `## ` headings name them (outside its code blocks) */
export function referenceSections(): string[] {
  const out: string[] = [];
  let code = false;
  for (const line of REFERENCE.split('\n')) {
    if (/^\s*```/.test(line)) code = !code;
    else if (!code && line.startsWith('## ')) out.push(line.slice(3).trim());
  }
  return out;
}

/** A diagnostic of the compiler, as the Code tab keeps it */
export interface ContextDiagnostic {
  level: string;
  message: string;
  rendered: string;
}

/** The contract being edited on an example's Code tab */
export interface CodeContext {
  /** The page's name in English, as the reference names the pages, such as `Counter` */
  page: string;
  /** The page's address, such as `#/counter/code` */
  route: string;
  /** The crate the compile service builds the source as */
  crate: string;
  source: string;
  /** Whether the source differs from the example's */
  edited: boolean;
  /** The compiler's diagnostics of this source, empty when there are none or it was not compiled */
  diagnostics: ContextDiagnostic[];
  /** The page's language: an assistant asked in Chinese answers in Chinese */
  locale: 'en' | 'zh-CN';
  /** Whether to copy the reference before the contract (not for a conversation that already has it) */
  withReference: boolean;
}

/** A fence longer than any run of backticks in `body`, so that the source's own ``` blocks stay inside it */
function fence(body: string): string {
  let longest = 2;
  for (const run of body.match(/`+/g) ?? []) longest = Math.max(longest, run.length);
  return '`'.repeat(longest + 1);
}

/**
 * What the Code tab copies: the reference (unless the conversation has it), then the contract with its page and crate, the
 * compiler's errors and warnings, and what the assistant is to do. The text around the source is English, as the reference
 * is; the last line asks for answers in the page's language.
 */
export function codeContext(ctx: CodeContext): string {
  const parts: string[] = ctx.withReference ? [REFERENCE.trimEnd(), '', '---', ''] : [];
  const features = STANDARDS_CRATES.has(ctx.crate) ? ', which has the contract SDK\'s `standards` feature' : ', on the plain contract SDK';
  parts.push(
    '# My contract',
    '',
    `I am working on this contract in the Ksana playground, on its ${ctx.page} page (https://playground.ksana.network/${ctx.route}). ` +
      `The compile service builds it as the \`src/lib.rs\` of the crate \`${ctx.crate}\`${features}. ` +
      (ctx.edited ? 'It is my edit of the example.' : 'It is the example as published.'),
    '',
  );
  const code = fence(ctx.source);
  parts.push(`${code}rust`, ctx.source.trimEnd(), code, '');
  if (ctx.diagnostics.length > 0) {
    const shown = ctx.diagnostics.slice(0, MAX_CONTEXT_DIAGNOSTICS);
    let rendered = shown.map((d) => (d.rendered || `${d.level}: ${d.message}`).trimEnd()).join('\n\n');
    const more = ctx.diagnostics.length - shown.length;
    if (more > 0) rendered += `\n\n(${more} more not shown)`;
    const block = fence(rendered);
    parts.push('## What the compiler said', '', `${block}text`, rendered, block, '');
  }
  const errors = ctx.diagnostics.some((d) => d.level === 'error');
  const reference = ctx.withReference ? 'the reference and the contract above' : 'the contract above with the Ksana contract reference I gave you earlier';
  parts.push(
    '## What to do',
    '',
    errors
      ? `Read ${reference}, then fix the compiler's errors and give me the whole corrected \`src/lib.rs\`, unless I ask for something else below.`
      : `Read ${reference}, then do what I ask below, following the reference's rules, and give me the whole \`src/lib.rs\` whenever you change it.`,
  );
  if (ctx.locale === 'zh-CN') parts.push('', 'Answer in Simplified Chinese; keep code, identifiers and revert reasons in English.');
  return `${parts.join('\n')}\n`;
}
