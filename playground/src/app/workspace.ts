/**
 * The Code tab's state for each example: the source as edited (kept in this browser), the latest build of it (compiled by the
 * service, or a module built locally and uploaded) and the compiler's diagnostics. A build remembers the ABI block of the source
 * it came from, so a deployment of it gets forms that match.
 */
import { useEffect } from 'react';
import { sha256, toHex } from '@ksana/sdk';
import { abiLines } from '../abi/parse';
import { compile, compilerAvailable, type Diagnostic } from '../compile/client';
import { endpoints } from '../chain/deployment';
import { validateWasm } from '../chain/ops';
import { createStore, loadPref, savePref, useStore } from './store';

export interface Build {
  wasm: Uint8Array;
  sha256: string;
  /** The ABI block of the source it was built from (null: the source declares none, or it was uploaded) */
  abi: string | null;
  /** The source it was built from (null for an uploaded module) */
  source: string | null;
  at: number;
  ms: number;
  /** The uploaded file's name */
  file?: string;
}

export interface Workspace {
  source: string;
  build: Build | null;
  diagnostics: Diagnostic[];
  /** The source the diagnostics are of (null before a build) */
  checked: string | null;
  compiling: boolean;
  error: unknown;
}

const stores = new Map<string, ReturnType<typeof createStore<Workspace>>>();
const originals = new Map<string, string>();

const sourceKey = (example: string) => `source:${example}`;

function storeOf(example: string, original: string) {
  let store = stores.get(example);
  if (!store) {
    originals.set(example, original);
    const saved = loadPref<string | null>(sourceKey(example), null, (v): v is string | null => v === null || typeof v === 'string');
    store = createStore<Workspace>({ source: saved ?? original, build: null, diagnostics: [], checked: null, compiling: false, error: null });
    stores.set(example, store);
  }
  return store;
}

let saveTimer: ReturnType<typeof setTimeout> | undefined;

export function setSource(example: string, original: string, source: string): void {
  const store = storeOf(example, original);
  store.update((w) => ({ ...w, source }));
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => savePref(sourceKey(example), source === originals.get(example) ? null : source), 400);
}

export function resetSource(example: string, original: string): void {
  const store = storeOf(example, original);
  store.update((w) => ({ ...w, source: original, diagnostics: [], checked: null, error: null }));
  savePref(sourceKey(example), null);
}

/** The ABI block of a source, as a source of its own that parseAbi reads; null without one */
export function abiBlock(source: string): string | null {
  try {
    const lines = abiLines(source);
    if (!lines) return null;
    return ['//! ```abi', ...lines.map((l) => `//! ${l.text}`), '//! ```', ''].join('\n');
  } catch {
    return null;
  }
}

let controller: AbortController | null = null;

/** Builds the example's current source with the compile service, as the example's crate */
export async function compileSource(example: string, original: string, crate: string): Promise<void> {
  const store = storeOf(example, original);
  const source = store.get().source;
  controller?.abort();
  const mine = new AbortController();
  controller = mine;
  store.update((w) => ({ ...w, compiling: true, error: null }));
  try {
    const result = await compile(endpoints().compile, source, crate, mine.signal);
    if (controller !== mine) return;
    store.update((w) => ({
      ...w,
      compiling: false,
      diagnostics: result.diagnostics,
      checked: source,
      build: result.ok ? { wasm: result.wasm, sha256: result.sha256, abi: abiBlock(source), source, at: Date.now(), ms: result.ms } : w.build,
    }));
  } catch (error) {
    if (controller !== mine) return;
    store.update((w) => ({ ...w, compiling: false, error }));
  }
}

/** Uses a module built elsewhere (the local build command); its forms come from the current source's ABI block */
export async function uploadModule(example: string, original: string, file: File): Promise<void> {
  const store = storeOf(example, original);
  try {
    const wasm = new Uint8Array(await file.arrayBuffer());
    await validateWasm(wasm);
    const source = store.get().source;
    store.update((w) => ({
      ...w,
      error: null,
      build: { wasm, sha256: toHex(sha256(wasm)), abi: abiBlock(source), source: null, at: Date.now(), ms: 0, file: file.name },
    }));
  } catch (error) {
    store.update((w) => ({ ...w, error }));
  }
}

export function useWorkspace(example: string, original: string): Workspace {
  return useStore(storeOf(example, original));
}

export function workspaceOf(example: string, original: string): Workspace {
  return storeOf(example, original).get();
}

const healthStore = createStore<Record<string, boolean | undefined>>({});

/** Whether the compile service answers (checked once per endpoint and visit) */
export function useCompilerAvailable(): boolean | undefined {
  const health = useStore(healthStore);
  const endpoint = endpoints().compile;
  useEffect(() => {
    if (!endpoint || endpoint in healthStore.get()) return;
    healthStore.update((h) => ({ ...h, [endpoint]: undefined }));
    void compilerAvailable(endpoint).then((ok) => healthStore.update((h) => ({ ...h, [endpoint]: ok })));
  }, [endpoint]);
  if (!endpoint) return false;
  return health[endpoint];
}
