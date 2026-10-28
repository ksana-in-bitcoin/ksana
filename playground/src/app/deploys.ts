/**
 * A deployment one page asks for: the example's deploy dialog opens on its App tab with some of `init`'s arguments filled in, as
 * the Asset page asks the AMM's for a pool of its asset, or a pool that cannot trade for one with its sides mended.
 */
import type { Value } from '../abi/types';
import { navigate, routeStore } from './route';
import { createStore } from './store';

export interface DeployRequest {
  example: string;
  /** init's arguments by name, over the example's own presets */
  presets: Record<string, Value>;
}

export const deployRequestStore = createStore<DeployRequest | null>(null);

export function requestDeploy(example: string, presets: Record<string, Value>): void {
  deployRequestStore.set({ example, presets });
  const route = routeStore.get();
  if (route.example !== example) navigate({ example, tab: 'app', contract: null });
  else if (route.tab !== 'app') navigate({ tab: 'app' }, true);
}
