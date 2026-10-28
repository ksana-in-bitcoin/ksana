/**
 * Where the playground finds its node, faucet and compile service.
 *
 * A hosted copy has `deployment.json` next to `index.html` (hosted/deployment.json, which the build of the copy at
 * playground.ksana.network puts there, vite.config.ts):
 *
 *   { "chain": "signet", "rpc": "https://signet.ksana.network/rpc", "faucet": "https://signet.ksana.network/faucet",
 *     "compile": "https://signet.ksana.network/compile", "explorer": "https://signet.ksana.network/",
 *     "genesis": "<genesis block hash>", "instances": { "amm": "<contract ID>" } }
 *
 * `instances` names contracts the operator deployed for everyone to use (a shared pool, a shared lending market) on the chain whose
 * genesis block is `genesis`, and the page offers them only there: a network started again from genesis keeps its chain ID but
 * not its contracts (without `genesis` none are offered). A copy without
 * the file (the development server) takes `?rpc=`, `?compile=`, `?faucet=` and `?explorer=` from the address bar instead. The
 * endpoints can also be changed in the page's settings, which are kept in this browser and win over both.
 */
import { loadPref, savePref } from '../app/store';

export type ChainName = 'devnet' | 'signet';

export interface Deployment {
  chain: ChainName;
  rpc: string;
  faucet?: string;
  compile?: string;
  explorer?: string;
  /** The hash of the genesis block of the chain `instances` are on (hex) */
  genesis?: string;
  instances?: Record<string, string>;
}

export interface Endpoints {
  rpc: string;
  faucet: string;
  compile: string;
  explorer: string;
}

const ID = /^[0-9a-f]{64}$/;

export function parseDeployment(value: unknown): Deployment | null {
  if (typeof value !== 'object' || value === null) return null;
  const v = value as Record<string, unknown>;
  const text = (x: unknown) => (typeof x === 'string' && x.trim() !== '' ? x.trim() : undefined);
  const rpc = text(v.rpc);
  if (!rpc || (v.chain !== 'devnet' && v.chain !== 'signet')) return null;
  const out: Deployment = { chain: v.chain, rpc };
  const faucet = text(v.faucet);
  const compile = text(v.compile);
  const explorer = text(v.explorer);
  if (faucet) out.faucet = faucet;
  if (compile) out.compile = compile;
  if (explorer) out.explorer = explorer;
  if (typeof v.genesis === 'string' && ID.test(v.genesis.toLowerCase())) out.genesis = v.genesis.toLowerCase();
  if (typeof v.instances === 'object' && v.instances !== null) {
    const instances: Record<string, string> = {};
    for (const [example, id] of Object.entries(v.instances as Record<string, unknown>)) {
      if (/^[a-z0-9-]{1,32}$/.test(example) && typeof id === 'string' && ID.test(id.toLowerCase())) instances[example] = id.toLowerCase();
    }
    out.instances = instances;
  }
  return out;
}

let deployment: Deployment | null = null;

export function currentDeployment(): Deployment | null {
  return deployment;
}

/** Reads `deployment.json` before the first render; a copy without it has no deployment */
export async function loadDeployment(): Promise<void> {
  if (typeof window === 'undefined') return;
  try {
    const res = await fetch(new URL('deployment.json', window.location.href), { cache: 'no-cache' });
    if (res.ok) deployment = parseDeployment(await res.json());
  } catch {
    deployment = null;
  }
}

function fromAddressBar(): Partial<Endpoints> {
  if (typeof window === 'undefined') return {};
  const params = new URLSearchParams(window.location.search);
  const out: Partial<Endpoints> = {};
  for (const key of ['rpc', 'faucet', 'compile', 'explorer'] as const) {
    const value = params.get(key)?.trim();
    if (value) out[key] = value;
  }
  return out;
}

function isEndpoints(value: unknown): value is Partial<Endpoints> {
  return typeof value === 'object' && value !== null && Object.values(value).every((v) => typeof v === 'string');
}

/** The endpoints in use: the page's settings, then the deployment, then (without a deployment) the address bar */
export function endpoints(): Endpoints {
  const saved = loadPref<Partial<Endpoints>>('endpoints', {}, isEndpoints);
  const d = deployment;
  const bar = d ? {} : fromAddressBar();
  return {
    rpc: saved.rpc ?? d?.rpc ?? bar.rpc ?? '',
    faucet: saved.faucet ?? d?.faucet ?? bar.faucet ?? '',
    compile: saved.compile ?? d?.compile ?? bar.compile ?? '',
    explorer: saved.explorer ?? d?.explorer ?? bar.explorer ?? '',
  };
}

/** Keeps the endpoints that differ from the hosted defaults; an empty object goes back to them */
export function saveEndpoints(next: Partial<Endpoints>): void {
  const clean: Partial<Endpoints> = {};
  for (const [k, v] of Object.entries(next) as [keyof Endpoints, string | undefined][]) {
    if (v !== undefined && v.trim() !== '') clean[k] = v.trim();
  }
  savePref('endpoints', Object.keys(clean).length ? clean : null);
}

/** An endpoint as an absolute URL (paths like `/rpc` are relative to the page) */
export function resolveUrl(url: string): string {
  return new URL(url, typeof window === 'undefined' ? 'http://localhost/' : window.location.href).toString();
}

/** A link into the block explorer (the web page's hash routes), or null without an explorer */
export function explorerLink(kind: 'op' | 'contract' | 'account' | 'block', id: string): string | null {
  const base = endpoints().explorer;
  if (!base) return null;
  try {
    const url = new URL(resolveUrl(base));
    url.hash = `#/${kind}/${id}`;
    return url.toString();
  } catch {
    return null;
  }
}
