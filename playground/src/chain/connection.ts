/**
 * The node the playground talks to: its parameters decide the network (devnet: regtest addresses; signet: tb1 addresses), and the
 * playground refuses any other network (it deploys example contracts and spends test KSA only).
 */
import { DEVNET, KsanaRpcClient, SIGNET, toHex, type BtcNetwork, type ChainInfo, type Params } from '@ksana/sdk';
import { createStore, useStore } from '../app/store';
import { PageNode } from './node';
import { endpointKey } from './ops';
import { endpoints, resolveUrl, type ChainName } from './deployment';

export interface Connection {
  /** The node's URL as configured (may be relative) */
  url: string;
  node: KsanaRpcClient;
  params: Params;
  /**
   * The hash of the node's genesis block (hex). With the chain ID it names the chain: a devnet or the signet started again from
   * genesis keeps its chain ID, so what the page keeps in this browser for a chain (its contracts, the journal) is kept under both.
   */
  genesis: string;
  network: BtcNetwork;
  chain: ChainName;
  /** A fingerprint of the endpoint and chain, for the journal (never the URL) */
  endpoint: string;
}

export type ConnectionState =
  | { state: 'none' }
  | { state: 'connecting'; url: string }
  | { state: 'ready'; connection: Connection }
  | { state: 'error'; url: string; error: string };

export const connectionStore = createStore<ConnectionState>({ state: 'none' });
export const tipStore = createStore<ChainInfo | null>(null);

let generation = 0;
let tipTimer: ReturnType<typeof setTimeout> | undefined;

export function networkOf(params: Params): { chain: ChainName; network: BtcNetwork } | null {
  if (params.chain_id === DEVNET.chain_id) return { chain: 'devnet', network: 'regtest' };
  if (params.chain_id === SIGNET.chain_id) return { chain: 'signet', network: 'signet' };
  return null;
}

export class UnsupportedNetworkError extends Error {
  constructor(readonly chainId: number) {
    super(`chain ${chainId}`);
    this.name = 'UnsupportedNetworkError';
  }
}

async function pollTip(id: number, node: KsanaRpcClient) {
  if (id !== generation) return;
  try {
    const info = await node.getChainInfo();
    if (id === generation) tipStore.set(info);
  } catch {
    /* the next poll tries again; a node that stays away shows as a stale height */
  }
  if (id === generation) tipTimer = setTimeout(() => void pollTip(id, node), 10_000);
}

/** Connects to the configured node (again); every connection replaces the previous one */
export async function connect(url = endpoints().rpc): Promise<void> {
  const id = ++generation;
  if (tipTimer) clearTimeout(tipTimer);
  tipStore.set(null);
  if (!url) {
    connectionStore.set({ state: 'none' });
    return;
  }
  connectionStore.set({ state: 'connecting', url });
  try {
    const node = new PageNode(resolveUrl(url), { timeoutMs: 20_000 });
    const [params, genesis] = await Promise.all([node.getParams(), node.getBlock(0)]);
    const net = networkOf(params);
    if (!net) throw new UnsupportedNetworkError(params.chain_id);
    if (id !== generation) return;
    connectionStore.set({
      state: 'ready',
      connection: {
        url, node, params, genesis: toHex(genesis.hash), network: net.network, chain: net.chain, endpoint: endpointKey(resolveUrl(url), params.chain_id),
      },
    });
    void pollTip(id, node);
  } catch (e) {
    if (id !== generation) return;
    connectionStore.set({ state: 'error', url, error: e instanceof Error ? e.message : String(e) });
  }
}

export function currentConnection(): Connection | null {
  const s = connectionStore.get();
  return s.state === 'ready' ? s.connection : null;
}

export function useConnectionState(): ConnectionState {
  return useStore(connectionStore);
}

export function useConnection(): Connection | null {
  const s = useStore(connectionStore);
  return s.state === 'ready' ? s.connection : null;
}

export function useTip(): ChainInfo | null {
  return useStore(tipStore);
}
