/**
 * A node for the tests: built-in assets and contracts in memory, answering `ksana_getAsset` and read-only calls as the node does
 * (an unknown asset is the RPC's not-found error, a call of a contract it does not have fails with the VM's words).
 */
import { DEVNET, RPC_ERROR_CODES, RpcError, fromHex, toHex, type CallRequest, type Receipt } from '@ksana/sdk';
import type { Connection } from './connection';

export interface Fake {
  assets: Map<string, { name: string; decimals: number; issuer: string }>;
  /** Contracts by ID: what a call of a method answers (bytes), or an error; undefined for a method it does not have */
  contracts: Map<string, (method: string, args: Uint8Array) => Uint8Array | string | undefined>;
}

function receipt(success: boolean, output: Uint8Array, error: string | null): Receipt {
  return { op_id: null, success, error, gas_used: 0n, output, events: [], asset_changes: [] };
}

let endpoints = 0;

/** A connection to a node that knows `fake`'s assets and contracts; each has an endpoint of its own, so nothing it caches is shared */
export function fakeConnection(fake: Fake): Connection {
  const node = {
    getAsset: async (asset: string | Uint8Array) => {
      const id = typeof asset === 'string' ? asset : toHex(asset);
      const info = fake.assets.get(id);
      if (!info) throw new RpcError(RPC_ERROR_CODES.NOT_FOUND, 'asset does not exist');
      return {
        asset: fromHex(id, 32),
        name: info.name,
        symbol: info.name.slice(0, 16),
        decimals: info.decimals,
        total_supply: 0n,
        issuer: fromHex(info.issuer, 32),
        capabilities: [],
        paused: false,
        at: { height: 1n, hash: new Uint8Array(32) },
      };
    },
    call: async (request: CallRequest) => {
      const id = typeof request.contract === 'string' ? request.contract : toHex(request.contract);
      const contract = fake.contracts.get(id);
      if (!contract) return receipt(false, new Uint8Array(), 'contract does not exist');
      const answer = contract(request.method, request.args);
      if (answer === undefined) return receipt(false, new Uint8Array(), 'revert: unknown method');
      return typeof answer === 'string' ? receipt(false, new Uint8Array(), answer) : receipt(true, answer, null);
    },
  };
  endpoints += 1;
  return { url: '', node, params: DEVNET, genesis: 'ab'.repeat(32), network: 'regtest', chain: 'devnet', endpoint: `fake-${endpoints}` } as unknown as Connection;
}
