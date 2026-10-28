/**
 * Where the page is, in the address bar's hash: `#/` is the list of examples, `#/<example>[/<tab>][?c=<contract ID>]` an example
 * with one of its tabs, on one deployed contract (a shared link names the contract).
 */
import { createStore, useStore } from './store';

export const TABS = ['app', 'code', 'methods', 'activity'] as const;
export type Tab = (typeof TABS)[number];

export interface Route {
  example: string | null;
  tab: Tab;
  contract: string | null;
}

const ID = /^[0-9a-f]{64}$/;

export function parseRoute(hash: string): Route {
  const raw = hash.replace(/^#/, '');
  const [path = '', query = ''] = raw.split('?');
  const parts = path.split('/').filter(Boolean);
  const example = parts[0] && /^[a-z0-9-]{1,32}$/.test(parts[0]) ? parts[0] : null;
  const tab = (TABS as readonly string[]).includes(parts[1] ?? '') ? (parts[1] as Tab) : 'app';
  const c = new URLSearchParams(query).get('c')?.toLowerCase() ?? '';
  return { example, tab, contract: ID.test(c) ? c : null };
}

export function formatRoute(route: Route): string {
  if (!route.example) return '#/';
  const tab = route.tab === 'app' ? '' : `/${route.tab}`;
  return `#/${route.example}${tab}${route.contract ? `?c=${route.contract}` : ''}`;
}

export const routeStore = createStore<Route>(parseRoute(typeof window === 'undefined' ? '' : window.location.hash));

if (typeof window !== 'undefined') {
  window.addEventListener('hashchange', () => routeStore.set(parseRoute(window.location.hash)));
}

export function navigate(next: Partial<Route>, replace = false): void {
  const route = { ...routeStore.get(), ...next };
  const hash = formatRoute(route);
  if (typeof window !== 'undefined' && window.location.hash !== hash) {
    if (replace) window.history.replaceState(window.history.state, '', hash);
    else window.history.pushState(window.history.state, '', hash);
  }
  routeStore.set(route);
  if (!replace && typeof window !== 'undefined') window.scrollTo({ top: 0 });
}

/** The link to an example's contract that anyone can open */
export function shareLink(example: string, contract: string): string {
  const url = new URL(typeof window === 'undefined' ? 'http://localhost/' : window.location.href);
  url.search = '';
  url.hash = formatRoute({ example, tab: 'app', contract });
  return url.toString();
}

export function useRoute(): Route {
  return useStore(routeStore);
}
