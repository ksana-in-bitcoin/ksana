/**
 * The compile service (playground/compiler/server.mjs): `POST <compile endpoint>` with `{ "source": "<lib.rs>", "crate": "<example>" }`
 * builds the source as that example's crate of contracts/, as scripts/build-examples.mjs builds the examples, and answers
 *
 *   { "ok": true, "wasm": "<base64>", "sha256": "<hex>", "diagnostics": [...] }      the module, with any warnings
 *   { "ok": false, "diagnostics": [...] }                                              the compiler's errors
 *
 * or an HTTP error with `{ "error": "<reason>" }` (busy, too large, too many requests). Diagnostics are rustc's, with their
 * positions in the source. A request that gets no answer, or one that is not the service's, is a `CompileServiceError` with
 * no message: the page says the service does not answer.
 */
import { sha256, toHex } from '@ksana/sdk';
import { resolveUrl } from '../chain/deployment';
import { validateWasm } from '../chain/ops';

export interface Span {
  lineStart: number;
  lineEnd: number;
  columnStart: number;
  columnEnd: number;
  primary: boolean;
  label: string | null;
}

export interface Diagnostic {
  level: 'error' | 'warning' | 'note' | 'help';
  message: string;
  /** rustc's own text of it, as a terminal shows it */
  rendered: string;
  spans: Span[];
}

export type CompileResult =
  | { ok: true; wasm: Uint8Array; sha256: string; diagnostics: Diagnostic[]; ms: number }
  | { ok: false; diagnostics: Diagnostic[]; ms: number };

export class CompileServiceError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
    this.name = 'CompileServiceError';
  }
}

/** The most source the service takes */
export const MAX_SOURCE_BYTES = 128 * 1024;

function base64ToBytes(text: string): Uint8Array {
  const raw = atob(text);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

function parseDiagnostics(value: unknown): Diagnostic[] {
  if (!Array.isArray(value)) return [];
  const out: Diagnostic[] = [];
  for (const d of value.slice(0, 200)) {
    if (typeof d !== 'object' || d === null) continue;
    const r = d as Record<string, unknown>;
    const level = r.level === 'error' || r.level === 'warning' || r.level === 'note' || r.level === 'help' ? r.level : 'error';
    const spans = Array.isArray(r.spans) ? r.spans.flatMap((s): Span[] => {
      if (typeof s !== 'object' || s === null) return [];
      const x = s as Record<string, unknown>;
      const n = (v: unknown) => (typeof v === 'number' && Number.isSafeInteger(v) && v > 0 ? v : 1);
      return [{ lineStart: n(x.line_start), lineEnd: n(x.line_end), columnStart: n(x.column_start), columnEnd: n(x.column_end),
        primary: x.is_primary === true, label: typeof x.label === 'string' ? x.label : null }];
    }) : [];
    out.push({ level, message: typeof r.message === 'string' ? r.message : '', rendered: typeof r.rendered === 'string' ? r.rendered : '', spans });
  }
  return out;
}

export async function compile(endpoint: string, source: string, crate: string, signal?: AbortSignal): Promise<CompileResult> {
  if (new TextEncoder().encode(source).length > MAX_SOURCE_BYTES) throw new CompileServiceError('source too large', 413);
  const started = performance.now();
  let res: Response;
  try {
    res = await fetch(resolveUrl(endpoint), {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ source, crate }),
      ...(signal ? { signal } : {}),
    });
  } catch (e) {
    if (signal?.aborted) throw e;
    // No answer: the service is away or restarting (a deploy cuts off the builds it was running)
    throw new CompileServiceError('', 0);
  }
  let body: Record<string, unknown> = {};
  try {
    body = (await res.json()) as Record<string, unknown>;
  } catch {
    /* reported by status below */
  }
  // An answer without the service's own reason (a proxy's 502 while it restarts) has no message
  if (!res.ok) throw new CompileServiceError(typeof body.error === 'string' ? body.error : '', res.status);
  const ms = Math.round(performance.now() - started);
  const diagnostics = parseDiagnostics(body.diagnostics);
  if (body.ok !== true) return { ok: false, diagnostics, ms };
  if (typeof body.wasm !== 'string') throw new CompileServiceError('the service sent no module', res.status);
  const wasm = base64ToBytes(body.wasm);
  const hash = toHex(sha256(wasm));
  if (typeof body.sha256 === 'string' && body.sha256 !== hash) throw new CompileServiceError('the module arrived damaged', res.status);
  await validateWasm(wasm);
  return { ok: true, wasm, sha256: hash, diagnostics, ms };
}

/** Whether a compile service answers at `endpoint` */
export async function compilerAvailable(endpoint: string): Promise<boolean> {
  if (!endpoint) return false;
  try {
    const res = await fetch(`${resolveUrl(endpoint).replace(/\/$/, '')}/health`, { cache: 'no-cache' });
    return res.ok;
  } catch {
    return false;
  }
}
