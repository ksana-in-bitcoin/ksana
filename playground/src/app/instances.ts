/**
 * The deployed contracts the page knows for each example, per chain: the hosted copy's shared ones (deployment.json), the ones
 * deployed from this browser, and the ones opened by ID. A contract deployed from edited code keeps the ABI block of the source it
 * was built from, so the generated forms match it. Kept in localStorage; IDs only, never keys. A chain is its chain ID and its
 * genesis block: a devnet or the signet started again from genesis keeps its chain ID but none of the contracts before.
 */
import { DEVNET, SIGNET } from '@ksana/sdk';
import { createStore, loadPref, savePref, useStore } from './store';
import type { Connection } from '../chain/connection';
import { currentDeployment } from '../chain/deployment';

/** The chain a list is kept for: the node's chain ID and genesis block */
export type Chain = Pick<Connection, 'params' | 'genesis'>;

export type InstanceOrigin = 'shared' | 'mine' | 'opened';

export interface Instance {
  id: string;
  origin: InstanceOrigin;
  /** When it was added (ms) */
  at: number;
  /** The ABI block of the edited source it was deployed from; absent for the example as published */
  abi?: string;
}

const ID = /^[0-9a-f]{64}$/;
const MAX_PER_EXAMPLE = 20;

function isInstance(v: unknown): v is Instance {
  if (typeof v !== 'object' || v === null) return false;
  const i = v as Record<string, unknown>;
  return typeof i.id === 'string' && ID.test(i.id) && (i.origin === 'mine' || i.origin === 'opened') && typeof i.at === 'number' &&
    (i.abi === undefined || (typeof i.abi === 'string' && i.abi.length <= 16_384));
}

function isList(v: unknown): v is Instance[] {
  return Array.isArray(v) && v.every(isInstance);
}

// keys from before the genesis block was part of them (`instances:<chain ID>:<example>`) are not read: their chain is unknown
const chainKey = (chain: Chain) => `${chain.params.chain_id}:${chain.genesis}`;
const key = (chain: Chain, example: string) => `instances:${chainKey(chain)}:${example}`;
const currentKey = (chain: Chain, example: string) => `current:${chainKey(chain)}:${example}`;

/** Bumped on every change, so hooks read the lists again */
const versionStore = createStore(0);

export function instancesOf(chain: Chain, example: string): Instance[] {
  const own = loadPref<Instance[]>(key(chain, example), [], isList);
  const deployment = currentDeployment();
  // the operator's contracts are on the chain whose genesis block the deployment names, and only there
  const sharedId = deployment?.genesis === chain.genesis && chainIdOf(deployment.chain) === chain.params.chain_id
    ? deployment.instances?.[example] : undefined;
  const shared: Instance[] = sharedId ? [{ id: sharedId, origin: 'shared', at: 0 }] : [];
  return [...shared, ...own.filter((i) => i.id !== sharedId)];
}

function chainIdOf(chain: 'devnet' | 'signet'): number {
  return chain === 'signet' ? SIGNET.chain_id : DEVNET.chain_id;
}

export function addInstance(chain: Chain, example: string, instance: Instance): void {
  const own = loadPref<Instance[]>(key(chain, example), [], isList).filter((i) => i.id !== instance.id);
  savePref(key(chain, example), [instance, ...own].slice(0, MAX_PER_EXAMPLE));
  savePref(currentKey(chain, example), instance.id);
  versionStore.update((n) => n + 1);
}

export function forgetInstance(chain: Chain, example: string, id: string): void {
  const own = loadPref<Instance[]>(key(chain, example), [], isList);
  savePref(key(chain, example), own.filter((i) => i.id !== id));
  if (loadPref<string | null>(currentKey(chain, example), null) === id) savePref(currentKey(chain, example), null);
  versionStore.update((n) => n + 1);
}

export function selectInstance(chain: Chain, example: string, id: string | null): void {
  savePref(currentKey(chain, example), id);
  versionStore.update((n) => n + 1);
}

/** The instance chosen last, else the newest one, else the shared one */
export function currentInstance(chain: Chain, example: string): Instance | null {
  const list = instancesOf(chain, example);
  const chosen = loadPref<string | null>(currentKey(chain, example), null, (v): v is string | null => v === null || (typeof v === 'string' && ID.test(v)));
  return list.find((i) => i.id === chosen) ?? list.find((i) => i.origin !== 'shared') ?? list[0] ?? null;
}

export function useInstances(chain: Chain | null, example: string): { list: Instance[]; current: Instance | null } {
  useStore(versionStore);
  if (chain === null) return { list: [], current: null };
  return { list: instancesOf(chain, example), current: currentInstance(chain, example) };
}
