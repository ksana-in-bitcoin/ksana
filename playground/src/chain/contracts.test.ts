import { describe, expect, it } from 'vitest';
import { RPC_ERROR_CODES, RpcError, fromHex } from '@ksana/sdk';
import { accountText } from '../abi/text';
import type { Connection } from './connection';
import { knownContract, lookUpContract, writeKnowingContracts } from './contracts';

/** The pool the factory made on the signet: a contract whose ID is a valid key as well */
const POOL = '2a220d0a8c6910fb557785daf4fdfed5eaea6f4690aeaedd9e0acb0ac008efb6';
/** The tutorial's account on the signet, and its address */
const PERSON = 'b7edd1c3bdca216534c8bd7882ac7cd0346d4508c9f3b6e7eb59d2136f43f09d';
const PERSON_ADDRESS = 'tb1pklkarsaaegsk2dxgh4ug9tru6q6x63gge8emdeltt8fpxm6r7zwse04hlm';

let nodes = 0;

/**
 * A node whose index knows `contracts` and answers `-32004` for any other account, or answers every question with `error`; each
 * has an endpoint of its own, as the answers are kept by node for the page's life
 */
function nodeOf(contracts: string[], error?: unknown) {
  const asked: string[] = [];
  const node = {
    getContract: async (contract: string) => {
      asked.push(contract);
      if (error !== undefined) throw error;
      if (!contracts.includes(contract)) throw new RpcError(RPC_ERROR_CODES.NOT_FOUND, 'contract does not exist');
      return { contract: fromHex(contract, 32) };
    },
  };
  nodes += 1;
  return { conn: { endpoint: `contracts-${nodes}`, network: 'signet', node } as unknown as Connection, asked };
}

describe('which accounts are contracts', () => {
  it('asks the node once for each account and keeps the answer', async () => {
    const { conn, asked } = nodeOf([POOL]);
    expect(knownContract(conn, POOL)).toBeNull();
    // two questions at once share the one sent, in hex or in bytes
    expect(await Promise.all([lookUpContract(conn, POOL), lookUpContract(conn, fromHex(POOL, 32))])).toEqual([true, true]);
    expect(await lookUpContract(conn, PERSON)).toBe(false);
    expect(await lookUpContract(conn, POOL.toUpperCase())).toBe(true);
    expect(await lookUpContract(conn, PERSON)).toBe(false);
    expect(asked).toEqual([POOL, PERSON]);
    expect(knownContract(conn, fromHex(POOL, 32))).toBe(true);
    expect(knownContract(conn, PERSON)).toBe(false);
  });

  it('keeps the answers of each node apart', async () => {
    const one = nodeOf([POOL]);
    const other = nodeOf([]);
    expect(await lookUpContract(one.conn, POOL)).toBe(true);
    expect(knownContract(other.conn, POOL)).toBeNull();
    expect(await lookUpContract(other.conn, POOL)).toBe(false);
    expect(knownContract(one.conn, POOL)).toBe(true);
  });

  it('asks a node without the index no more, and knows none of its accounts', async () => {
    const { conn, asked } = nodeOf([], new RpcError(RPC_ERROR_CODES.METHOD_NOT_FOUND, 'Method not found'));
    expect(await lookUpContract(conn, POOL)).toBeNull();
    expect(await lookUpContract(conn, POOL)).toBeNull();
    expect(await lookUpContract(conn, PERSON)).toBeNull();
    expect(asked).toEqual([POOL]);
    expect(knownContract(conn, POOL)).toBeNull();
  });

  it('asks again about an account the node could not answer for', async () => {
    const { conn, asked } = nodeOf([], new Error('fetch failed'));
    expect(await lookUpContract(conn, POOL)).toBeNull();
    expect(knownContract(conn, POOL)).toBeNull();
    expect(await lookUpContract(conn, POOL)).toBeNull();
    expect(asked).toEqual([POOL, POOL]);
  });

  it('writes text naming accounts once the node has said which are contracts', async () => {
    const { conn, asked } = nodeOf([POOL]);
    const text = await writeKnowingContracts(conn, (isContract) =>
      [POOL, PERSON].map((hex) => accountText(fromHex(hex, 32), 'signet', isContract(hex))).join(' '));
    expect(text).toBe(`${POOL} ${PERSON_ADDRESS}`);
    expect(asked).toEqual([POOL, PERSON]);
    // asked once: the next text knows them
    expect(await writeKnowingContracts(conn, (isContract) => isContract(fromHex(POOL, 32)))).toBe(true);
    expect(asked).toEqual([POOL, PERSON]);
  });
});
