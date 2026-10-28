/** Which example a deployed contract is: the one whose distinctive view answers and decodes with its ABI. */
import { readMethod } from '../chain/views';
import type { Connection } from '../chain/connection';
import { EXAMPLES, exampleAbi, type ExampleId } from '.';

const PROBES: Record<ExampleId, string> = {
  counter: 'get', token: 'info', ks20: 'metadata', nft: 'collection', amm: 'tokens', lending: 'market', multisig: 'wallet', vesting: 'count',
};

export async function identify(conn: Connection, contract: string): Promise<ExampleId | null> {
  const results = await Promise.all(EXAMPLES.map(async (def) => {
    const abi = exampleAbi(def);
    if (!abi) return null;
    try {
      await readMethod(conn, contract, abi, PROBES[def.id], []);
      return def.id;
    } catch {
      return null;
    }
  }));
  return results.find((r) => r !== null) ?? null;
}
