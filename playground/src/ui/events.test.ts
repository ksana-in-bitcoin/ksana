import { describe, expect, it } from 'vitest';
import { RpcError, RPC_ERROR_CODES, fromHex, toHex, type OpPosition } from '@ksana/sdk';
import { parseAbi } from '../abi/parse';
import type { Connection } from '../chain/connection';
import { readEvents } from './Events';

const POOL = 'aa'.repeat(32);
const TOKEN = 'bb'.repeat(32);
const ABI = parseAbi(['//! ```abi', '//! event swap(trader: Account, amount_out: u128)', '//! ```'].join('\n'))!;

/** The Borsh bytes of the event `("swap", trader, amount_out)` */
function swap(amount: number): Uint8Array {
  const name = new TextEncoder().encode('swap');
  const out = new Uint8Array(4 + name.length + 32 + 16);
  out.set([name.length, 0, 0, 0]);
  out.set(name, 4);
  out.fill(7, 4 + name.length, 4 + name.length + 32);
  out[4 + name.length + 32] = amount;
  return out;
}

type Listed = [id: string, height: number, slot: number];

/**
 * A node whose listings hold the ops `byAccount` names (`more`: the accounts whose listing goes on below them), each op's receipt
 * the events of `receipts`. `deployed` is its index of the standards' events, the deployment of each contract that declared a
 * standard; without it the node answers the index's methods as one from before D-120.
 */
function nodeOf(byAccount: Record<string, Listed[]>, receipts: Record<string, { contract: string; data: Uint8Array }[]>,
  { deployed, more = [] }: { deployed?: Record<string, Listed>; more?: string[] } = {}) {
  const listed: string[] = [];
  const asked: string[] = [];
  const ref = ([id, height, slot]: Listed) => ({
    op_id: fromHex(id, 32), status: 'fast_confirmed', index: 0,
    block: { hash: new Uint8Array(32), height: BigInt(height) },
    position: { height: BigInt(height), slot, entry: 0 },
  });
  const below = (p: OpPosition, q: OpPosition) => p.height < q.height || (p.height === q.height && (p.slot < q.slot || (p.slot === q.slot && p.entry < q.entry)));
  const at = { hash: new Uint8Array(32), height: 100n };
  const node = {
    getAccountOps: async (account: string, { limit }: { limit: number }) => {
      listed.push(account);
      const ops = (byAccount[account] ?? []).slice(0, limit).map(ref);
      return { ops, next: more.includes(account) ? ops[ops.length - 1]!.position : null, at };
    },
    getOpStatus: async (id: Uint8Array) => ({
      receipt: { events: (receipts[toHex(id)] ?? []).map((e) => ({ contract: fromHex(e.contract, 32), data: e.data })) },
    }),
    getContract: async (contract: string) => {
      asked.push(contract);
      if (!deployed) throw new RpcError(RPC_ERROR_CODES.METHOD_NOT_FOUND, 'Method not found');
      const op = deployed[contract];
      return { declared: op ? ref(op).position : null };
    },
    // The whole chain: every op listed or deployed, newest first
    getLatestOps: async ({ before, limit }: { before: OpPosition; limit: number }) => {
      const all = [...Object.values(byAccount).flat(), ...Object.values(deployed ?? {})].map(ref);
      const ops = all.filter((r) => below(r.position, before)).sort((x, y) => (below(x.position, y.position) ? 1 : -1)).slice(0, limit);
      return { ops, next: null, at };
    },
  };
  return { conn: { node } as unknown as Connection, listed, asked };
}

const op = (n: number) => n.toString(16).padStart(2, '0').repeat(32);

describe('a contract\'s recent events', () => {
  it('reads the ops that reached it through a token side too, newest first, each once', async () => {
    const { conn, listed } = nodeOf({
      // a swap called on the pool, and two one-step swaps through the token, one also a call of the pool by its own listing
      [POOL]: [[op(3), 12, 0], [op(1), 10, 0]],
      [TOKEN]: [[op(3), 12, 0], [op(2), 11, 1], [op(4), 11, 0]],
    }, {
      [op(1)]: [{ contract: POOL, data: swap(1) }],
      [op(2)]: [{ contract: TOKEN, data: swap(9) }, { contract: POOL, data: swap(2) }],
      [op(3)]: [{ contract: POOL, data: swap(3) }],
      [op(4)]: [{ contract: TOKEN, data: swap(9) }],
    });
    const events = await readEvents(conn, POOL, ABI, 10, [TOKEN]);
    expect(listed).toEqual([POOL, TOKEN]);
    expect(events.map((e) => [e.opId, e.values.amount_out])).toEqual([[op(3), 3n], [op(2), 2n], [op(1), 1n]]);
  });

  it('keeps the newest `limit` ops of them all', async () => {
    const { conn } = nodeOf({ [POOL]: [[op(1), 10, 0]], [TOKEN]: [[op(3), 12, 0], [op(2), 11, 0]] }, {
      [op(1)]: [{ contract: POOL, data: swap(1) }], [op(2)]: [{ contract: POOL, data: swap(2) }], [op(3)]: [{ contract: POOL, data: swap(3) }],
    });
    expect((await readEvents(conn, POOL, ABI, 2, [TOKEN])).map((e) => e.opId)).toEqual([op(3), op(2)]);
    expect((await readEvents(conn, POOL, ABI, 2, [])).map((e) => e.opId)).toEqual([op(1)]);
  });

  it('adds the deployment of a contract that declared its standard, below every op listed', async () => {
    const { conn, asked } = nodeOf({ [TOKEN]: [[op(3), 12, 0], [op(2), 11, 0]] }, {
      [op(1)]: [{ contract: TOKEN, data: swap(1) }], [op(2)]: [{ contract: TOKEN, data: swap(2) }], [op(3)]: [{ contract: TOKEN, data: swap(3) }],
      [op(9)]: [{ contract: POOL, data: swap(9) }],
    }, { deployed: { [TOKEN]: [op(1), 10, 1], [POOL]: [op(9), 10, 0] } });
    expect((await readEvents(conn, TOKEN, ABI, 10, [])).map((e) => [e.opId, e.values.amount_out])).toEqual([[op(3), 3n], [op(2), 2n], [op(1), 1n]]);
    expect(asked).toEqual([TOKEN]);
  });

  it('shows the listings alone from a node without the index, or for a contract without a declaration', async () => {
    const receipts = { [op(1)]: [{ contract: TOKEN, data: swap(1) }], [op(2)]: [{ contract: TOKEN, data: swap(2) }] };
    const old = nodeOf({ [TOKEN]: [[op(2), 11, 0]] }, receipts);
    expect((await readEvents(old.conn, TOKEN, ABI, 10, [])).map((e) => e.opId)).toEqual([op(2)]);
    expect(old.asked).toEqual([TOKEN]);
    const none = nodeOf({ [TOKEN]: [[op(2), 11, 0]] }, receipts, { deployed: {} });
    expect((await readEvents(none.conn, TOKEN, ABI, 10, [])).map((e) => e.opId)).toEqual([op(2)]);
  });

  it('asks for no deployment when the listings fill the page or go on below it', async () => {
    const receipts = { [op(1)]: [{ contract: TOKEN, data: swap(1) }], [op(2)]: [{ contract: TOKEN, data: swap(2) }] };
    const deployed = { [TOKEN]: [op(1), 10, 0] as Listed };
    const full = nodeOf({ [TOKEN]: [[op(2), 11, 0]] }, receipts, { deployed });
    expect((await readEvents(full.conn, TOKEN, ABI, 1, [])).map((e) => e.opId)).toEqual([op(2)]);
    const going = nodeOf({ [TOKEN]: [[op(2), 11, 0]] }, receipts, { deployed, more: [TOKEN] });
    expect((await readEvents(going.conn, TOKEN, ABI, 10, [])).map((e) => e.opId)).toEqual([op(2)]);
    expect([...full.asked, ...going.asked]).toEqual([]);
  });
});
