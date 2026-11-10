/**
 * A deployment one page asks for: the example's deploy dialog opens on its App tab with some of `init`'s arguments filled in, as
 * the Asset page asks the AMM's for a pool of its asset, a pool that cannot trade for one with its sides mended, or the first
 * page's workbench for the example as it was set there.
 */
import type { Value } from '../abi/types';
import { navigate, routeStore } from './route';
import { createStore } from './store';

export interface DeployRequest {
  example: string;
  /** init's arguments by name, over the example's own presets */
  presets: Record<string, Value>;
  /**
   * The request is the whole of what was set (the workbench): the dialog starts on the published module even when this browser
   * holds a build of the example, and an argument left out starts empty instead of at the example's preset
   */
  exact: boolean;
}

export const deployRequestStore = createStore<DeployRequest | null>(null);

export function requestDeploy(example: string, presets: Record<string, Value>, exact = false): void {
  deployRequestStore.set({ example, presets, exact });
  const route = routeStore.get();
  if (route.example !== example) navigate({ example, tab: 'app', contract: null });
  else if (route.tab !== 'app') navigate({ tab: 'app' }, true);
}
