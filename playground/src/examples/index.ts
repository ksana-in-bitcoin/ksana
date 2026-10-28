/**
 * The examples: each is a contract in contracts/examples/ (its source is read from there at build time), its module as
 * scripts/build-examples.mjs built it, and a page that uses it. The source's ABI block gives the generic forms and decodes
 * results and events; the page is written against the same block.
 */
import type { ComponentType } from 'react';
import { DEVNET, ksaAssetId, type BtcNetwork } from '@ksana/sdk';
import { parseAbi } from '../abi/parse';
import type { Abi, Field, Value } from '../abi/types';
import type { Connection } from '../chain/connection';
import type { InitProblem } from '../chain/ids';
import { ammInitProblems } from './amm/token';
import type { AppProps } from './kit';
import builds from './wasm/builds.json';

import counterSource from '../../../contracts/examples/playground-counter/src/lib.rs?raw';
import tokenSource from '../../../contracts/examples/mintable-token/src/lib.rs?raw';
import ks20Source from '../../../contracts/examples/token/src/lib.rs?raw';
import nftSource from '../../../contracts/examples/nft/src/lib.rs?raw';
import ammSource from '../../../contracts/examples/amm/src/lib.rs?raw';
import lendingSource from '../../../contracts/examples/lending/src/lib.rs?raw';
import multisigSource from '../../../contracts/examples/multisig/src/lib.rs?raw';
import vestingSource from '../../../contracts/examples/vesting/src/lib.rs?raw';

import counterWasm from './wasm/playground-counter.wasm?url';
import tokenWasm from './wasm/mintable-token.wasm?url';
import ks20Wasm from './wasm/token.wasm?url';
import nftWasm from './wasm/nft.wasm?url';
import ammWasm from './wasm/amm.wasm?url';
import lendingWasm from './wasm/lending.wasm?url';
import multisigWasm from './wasm/multisig.wasm?url';
import vestingWasm from './wasm/vesting.wasm?url';

import CounterApp from './counter/App';
import TokenApp from './token/App';
import Ks20App from './ks20/App';
import NftApp from './nft/App';
import AmmApp from './amm/App';
import LendingApp from './lending/App';
import MultisigApp from './multisig/App';
import VestingApp from './vesting/App';

export type ExampleId = 'counter' | 'token' | 'ks20' | 'nft' | 'amm' | 'lending' | 'multisig' | 'vesting';

export interface PresetContext {
  /** The connected wallet's account, when there is one */
  me: Uint8Array | null;
  network: BtcNetwork | null;
}

export interface BuildInfo {
  source: string;
  wasm: string;
  bytes: number;
}

export interface ExampleDef {
  id: ExampleId;
  /** The crate in contracts/examples/ */
  crate: string;
  source: string;
  wasmUrl: string;
  build: BuildInfo | null;
  App: ComponentType<AppProps>;
  /** init's arguments as the deploy form starts them, by name; a name left out starts empty */
  presets?: (ctx: PresetContext) => Record<string, Value>;
  /** The examples this one builds on (an AMM pairs an asset a contract issued, or a KS-20 token) */
  uses?: ExampleId[];
  /** What the example itself finds wrong with init's arguments before a deployment, besides asset IDs that name no asset */
  check?: (conn: Connection, abi: Abi, params: Field[], values: Value[]) => Promise<InitProblem[]>;
}

const known = builds as Record<string, BuildInfo | undefined>;
const KSA = ksaAssetId();
const UNIT = 100_000_000n;
/** The lending market's fixed point (contracts/examples/lending: prices are loan units per collateral unit times 10^18) */
const WAD = 10n ** 18n;

export const EXAMPLES: readonly ExampleDef[] = [
  { id: 'counter', crate: 'playground-counter', source: counterSource, wasmUrl: counterWasm, build: known['playground-counter'] ?? null, App: CounterApp },
  {
    id: 'token', crate: 'mintable-token', source: tokenSource, wasmUrl: tokenWasm, build: known['mintable-token'] ?? null, App: TokenApp,
    presets: () => ({ name: 'Play', symbol: 'PLAY', decimals: 8n, cap: 21_000_000n * UNIT, initial_supply: 1_000_000n * UNIT }),
  },
  {
    id: 'ks20', crate: 'token', source: ks20Source, wasmUrl: ks20Wasm, build: known.token ?? null, App: Ks20App,
    // the deployer gets the supply and, with a wallet connected, can mint more up to the cap
    presets: ({ me }): Record<string, Value> => ({
      name: 'Demo Token', symbol: 'DEMO', decimals: 8n, supply: 1_000_000n * UNIT, cap: 21_000_000n * UNIT, ...(me ? { minter: me } : {}),
    }),
  },
  {
    id: 'nft', crate: 'nft', source: nftSource, wasmUrl: nftWasm, build: known.nft ?? null, App: NftApp,
    presets: () => ({ name: 'Ksana Kites', symbol: 'KITE', max_supply: 10_000n }),
  },
  {
    id: 'amm', crate: 'amm', source: ammSource, wasmUrl: ammWasm, build: known.amm ?? null, App: AmmApp, uses: ['token', 'ks20'],
    presets: () => ({ side_a: { variant: 'Asset', values: [KSA] }, side_b: { variant: 'Asset', values: [] } }),
    check: ammInitProblems,
  },
  {
    id: 'lending', crate: 'lending', source: lendingSource, wasmUrl: lendingWasm, build: known.lending ?? null, App: LendingApp, uses: ['token'],
    // a market lending KSA against a token's asset (picked at deployment), at a price of 1 and moderate rates
    presets: () => ({
      loan_asset: KSA,
      price: WAD,
      params: {
        collateral_factor_bps: 7_500n, liquidation_threshold_bps: 8_000n, liquidation_bonus_bps: 500n, reserve_factor_bps: 1_000n,
        base_rate_bps: 200n, slope1_bps: 400n, kink_bps: 8_000n, slope2_bps: 6_000n,
      },
    }),
  },
  {
    id: 'multisig', crate: 'multisig', source: multisigSource, wasmUrl: multisigWasm, build: known.multisig ?? null, App: MultisigApp,
    presets: ({ me }): Record<string, Value> => (me ? { owners: [me], threshold: 1n } : { threshold: 1n }),
  },
  { id: 'vesting', crate: 'vesting', source: vestingSource, wasmUrl: vestingWasm, build: known.vesting ?? null, App: VestingApp, uses: ['token'] },
];

export function exampleById(id: string | null): ExampleDef | null {
  return EXAMPLES.find((e) => e.id === id) ?? null;
}

const abis = new Map<string, Abi | null>();

/** The ABI block of a source; null when it has none or it does not parse */
export function abiOf(source: string): Abi | null {
  let abi = abis.get(source);
  if (abi === undefined) {
    try {
      abi = parseAbi(source);
    } catch {
      abi = null;
    }
    abis.set(source, abi);
  }
  return abi;
}

export function exampleAbi(def: ExampleDef): Abi | null {
  return abiOf(def.source);
}

/** The published module of an example */
export async function exampleWasm(def: ExampleDef): Promise<Uint8Array> {
  const res = await fetch(def.wasmUrl);
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

/** Whether the network can run the examples' deployments without a funded wallet (the devnet's faucet is the operator) */
export function isDevnet(chainId: number): boolean {
  return chainId === DEVNET.chain_id;
}
