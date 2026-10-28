import assert from 'node:assert/strict';
import test from 'node:test';
import { RPC_ERROR_CODES, RpcError, SIGNET, accountOf, opId, toHex, verifyVmop } from '../../../sdk/ts/dist/src/index.js';
import { Cooldown, Dripper, FaucetError, formatKsa, handle, ksaUnits } from '../faucet.mjs';
import { RateLimit } from '../lib.mjs';

const SECRET = new Uint8Array(32).fill(7);
const ALICE = accountOf(new Uint8Array(32).fill(9));
const BOB = accountOf(new Uint8Array(32).fill(10));
const KSA = 100_000_000n;

/**
 * A node with an entry pool: `include(n)` lets a block include the pool's ops up to nonce n - 1. It suggests the fee
 * `suggested`, or with `suggested: null` the median of the lowest fees in its last 20 blocks, as `ksana_estimateFee` does.
 * Its refusals are ksana-mempool's: a fee below `floor`, an op at a nonce another op holds unless it pays more, and with
 * `full` set a full pool, whose lowest ops pay `full`.
 */
function fakeNode({ balance = 1_000n * KSA, chainId = SIGNET.chain_id, suggested = 5n } = {}) {
  const policy = (message) => new RpcError(RPC_ERROR_CODES.OP_REJECTED, `ksana_submitOp: ${message}`);
  const node = {
    nonce: 0n,
    balance,
    pool: new Map(),
    submitted: [],
    floor: 0n,
    full: null,
    suggested,
    /** The lowest fee in each block `include` made. */
    blocks: [],
    async getChainInfo() {
      return { chain_id: chainId };
    },
    async getAccount(account) {
      return { account, nonce: node.nonce, balances: [{ asset: new Uint8Array(32), amount: node.balance }] };
    },
    async estimateFee() {
      if (node.suggested !== null) return { path_b: { amount: node.suggested, per_kib: 0n } };
      const recent = node.blocks.slice(-20).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
      const median = recent.length === 0 ? 0n : recent[recent.length >> 1];
      return { path_b: { amount: median > 1n ? median : 1n, per_kib: 0n } };
    },
    async submitOp(op) {
      assert.equal(verifyVmop(op), 'valid');
      node.submitted.push(op);
      const id = toHex(opId(op));
      if (node.pool.has(id)) return { op_id: opId(op) };
      if (op.nonce < node.nonce) {
        throw new RpcError(RPC_ERROR_CODES.OP_INVALID, `ksana_submitOp: invalid op: nonce ${op.nonce} already used (account nonce is ${node.nonce})`);
      }
      if (op.fee.amount < node.floor) throw policy(`sponsored fee ${op.fee.amount} is below this node's minimum ${node.floor}`);
      const taken = [...node.pool].find(([, other]) => other.nonce === op.nonce);
      if (taken !== undefined) {
        if (op.fee.amount <= taken[1].fee.amount) {
          throw policy('an op with the same nonce already exists; replacing it needs a higher sponsored fee');
        }
        node.pool.delete(taken[0]);
      } else if (node.full !== null && op.fee.amount <= node.full) {
        throw policy('entry pool is full and this op stands below the ops it would have to evict');
      }
      node.pool.set(id, op);
      return { op_id: opId(op) };
    },
    async getOpStatus(id) {
      const op = node.pool.get(toHex(id));
      if (op === undefined) throw new RpcError(RPC_ERROR_CODES.NOT_FOUND, 'ksana_getOpStatus: unknown op');
      return { status: 'pending' };
    },
    include(upTo) {
      let lowest;
      for (const [id, op] of node.pool) {
        if (op.nonce < upTo) {
          node.pool.delete(id);
          node.balance -= op.action.amount + op.fee.amount;
          if (lowest === undefined || op.fee.amount < lowest) lowest = op.fee.amount;
        }
      }
      if (lowest !== undefined) node.blocks.push(lowest);
      node.nonce = upTo;
    },
  };
  return node;
}

test('KSA amounts parse to base units and print back', () => {
  assert.equal(ksaUnits('100'), 100n * KSA);
  assert.equal(ksaUnits('0.5'), KSA / 2n);
  assert.equal(ksaUnits('1.00000001'), KSA + 1n);
  for (const bad of ['', '0', '-1', '1.000000001', '1e3', 'abc']) assert.throws(() => ksaUnits(bad));
  assert.equal(formatKsa(100n * KSA), '100');
  assert.equal(formatKsa(KSA + KSA / 4n), '1.25');
});

test('a cooldown holds a key until it has passed', () => {
  let t = 0;
  const cooldown = new Cooldown(1_000, () => t);
  assert.equal(cooldown.remaining('a'), 0);
  cooldown.mark('a');
  t = 400;
  assert.equal(cooldown.remaining('a'), 600);
  t = 1_000;
  assert.equal(cooldown.remaining('a'), 0);
});

test('the faucet refuses a node of another network', async () => {
  const node = fakeNode({ chainId: 1001 });
  await assert.rejects(new Dripper({ rpc: node, secretKey: SECRET, amount: KSA }).init(), /chain 1001/);
});

test('transfers go out with consecutive nonces and the floor fee', async () => {
  const node = fakeNode();
  node.nonce = 3n;
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: 10n * KSA });
  await dripper.init();
  const results = await Promise.all([dripper.drip(ALICE), dripper.drip(ALICE), dripper.drip(ALICE)]);
  assert.deepEqual(results.map((r) => r.nonce), [3n, 4n, 5n]);
  for (const op of node.submitted) {
    assert.equal(op.chain_id, SIGNET.chain_id);
    assert.deepEqual(op.account, accountOf(SECRET));
    assert.deepEqual(op.fee, { kind: 'Token', amount: 1n });
    assert.equal(op.max_gas, 10_000n);
    assert.deepEqual(op.action, { kind: 'AssetTransfer', asset: new Uint8Array(32), to: ALICE, amount: 10n * KSA });
  }
  node.include(5n);
  await dripper.maintain();
  assert.deepEqual([...dripper.waiting.keys()], [5n], 'included transfers stop waiting');
});

test('the fee stays at the floor while the node suggests more', async () => {
  let t = 0;
  const node = fakeNode({ suggested: null });
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: KSA, now: () => t });
  await dripper.init();
  const fees = [];
  const drip = async () => {
    t += 120_000;
    fees.push((await dripper.drip(ALICE)).fee);
    node.include(node.nonce + 1n);
  };
  // Blocks with only the faucet's transfers: the suggestion follows them and stays at the floor
  for (let i = 0; i < 4; i += 1) await drip();
  assert.equal((await node.estimateFee()).path_b.amount, 1n);
  // Blocks of others paying more raise the median
  node.blocks.push(30n, 40n, 50n, 60n, 70n);
  assert.equal((await node.estimateFee()).path_b.amount, 30n);
  for (let i = 0; i < 2; i += 1) await drip();
  assert.deepEqual(fees, [1n, 1n, 1n, 1n, 1n, 1n]);
});

test('a transfer the node refuses at the floor fee pays its suggestion', async () => {
  const node = fakeNode({ suggested: 3n });
  node.floor = 3n;
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: KSA });
  await dripper.init();
  assert.equal((await dripper.drip(ALICE)).fee, 3n);
  assert.deepEqual(node.submitted.map((op) => op.fee.amount), [1n, 3n]);
  assert.equal(dripper.fee, 1n, 'the next transfer tries the floor again');
  node.suggested = 1n; // a suggestion no higher than the refused fee: the refusal stands
  await assert.rejects(dripper.drip(ALICE), (e) => e instanceof RpcError && e.code === RPC_ERROR_CODES.OP_REJECTED);
});

test('a transfer still waiting at its nonce is never replaced, as after a restart', async () => {
  const node = fakeNode({ suggested: 3n });
  const before = new Dripper({ rpc: node, secretKey: SECRET, amount: KSA });
  await before.init();
  const sent = await before.drip(ALICE); // answered with its op_id
  // The faucet restarts before a block includes it: it counts again from the account nonce
  const after = new Dripper({ rpc: node, secretKey: SECRET, amount: KSA });
  await after.init();
  await assert.rejects(after.drip(BOB), (e) => e instanceof RpcError && /same nonce already exists/.test(e.message));
  assert.deepEqual(node.submitted.map((op) => op.fee.amount), [1n, 1n], 'nothing signed at the suggested fee');
  assert.deepEqual([...node.pool.keys()], [toHex(sent.op_id)], "ALICE's transfer still waits");
  node.include(1n);
  assert.equal((await after.drip(BOB)).nonce, 1n, 'once a block includes it, the faucet counts on');
});

test('a fee raised for a full pool stops at the cap', async () => {
  const node = fakeNode({ suggested: 50n * KSA }); // a few blocks of high fees raised the suggestion to half a drip
  node.full = 1n; // while others' fee-1 transfers fill the pool
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: 100n * KSA });
  await dripper.init();
  assert.equal((await dripper.drip(ALICE)).fee, 1_000n);
  node.full = 1_000n; // a pool the cap does not get into: the request fails
  await assert.rejects(dripper.drip(BOB), /entry pool is full/);
  assert.deepEqual(node.submitted.map((op) => op.fee.amount), [1n, 1_000n, 1n, 1_000n]);
});

test('the balance is checked again before a transfer pays a raised fee', async () => {
  const node = fakeNode({ balance: 100n * KSA + 10n, suggested: 50n });
  node.full = 1n;
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: 100n * KSA });
  await dripper.init();
  // The node checks only that the balance covers the fees: a transfer at 50 would be taken, then fail and still pay
  await assert.rejects(dripper.drip(ALICE), (e) => e instanceof FaucetError && e.status === 503 && /empty/.test(e.message));
  assert.deepEqual(node.submitted.map((op) => op.fee.amount), [1n], 'nothing signed at the raised fee');
});

test('a nonce someone else used is counted again from the account', async () => {
  const node = fakeNode();
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: KSA });
  await dripper.init();
  node.nonce = 2n; // two transfers signed elsewhere were included
  const { nonce } = await dripper.drip(ALICE);
  assert.equal(nonce, 2n);
  assert.deepEqual(node.submitted.map((op) => op.nonce), [0n, 2n]);
});

test('the faucet says when it is empty or has too many transfers waiting', async () => {
  const node = fakeNode({ balance: 25n * KSA });
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: 10n * KSA });
  await dripper.init();
  await dripper.drip(ALICE);
  await dripper.drip(ALICE);
  await assert.rejects(dripper.drip(ALICE), (e) => e instanceof FaucetError && e.status === 503 && /empty/.test(e.message));

  const busy = new Dripper({ rpc: fakeNode(), secretKey: SECRET, amount: 1n });
  await busy.init();
  for (let i = 0; i < 48; i += 1) await busy.drip(ALICE);
  await assert.rejects(busy.drip(ALICE), (e) => e instanceof FaucetError && /waiting/.test(e.message));
});

test('transfers the node lost are submitted again, re-signed when the node asks for a higher fee', async () => {
  let t = 0;
  const node = fakeNode();
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: KSA, now: () => t });
  await dripper.init();
  await dripper.drip(ALICE);
  await dripper.drip(ALICE);
  const before = [...dripper.waiting.values()].map((w) => toHex(w.id));

  node.pool.clear(); // a main-chain switch emptied the pool
  t += 30_000;
  await dripper.maintain();
  assert.equal(node.pool.size, 0, 'not before a minute has passed');
  t += 30_000;
  await dripper.maintain();
  assert.deepEqual([...node.pool.keys()].sort(), [...before].sort(), 'the same transfers are back');

  node.pool.clear();
  node.floor = 15n; // the node's floor rose above the fee, and it suggests at least its floor
  node.suggested = 15n;
  t += 60_000;
  await dripper.maintain();
  const fees = [...dripper.waiting.values()].map((w) => w.fee);
  assert.deepEqual(fees, [15n, 15n]);
  assert.equal(node.pool.size, 2);
  assert.deepEqual([...node.pool.values()].map((op) => op.nonce).sort(), [0n, 1n]);
});

test('a lost transfer refused again is signed with a higher fee only up to the cap', async () => {
  let t = 0;
  const node = fakeNode({ suggested: 1n });
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: KSA, maxFee: 4n, now: () => t });
  await dripper.init();
  await dripper.drip(ALICE);
  const evict = () => {
    // A flood that stands at the transfer's own fee pushes it out of the pool and keeps it out
    node.pool.clear();
    node.full = dripper.waiting.get(0n).fee;
    t += 60_000;
  };
  const fees = [];
  for (let round = 0; round < 2; round += 1) {
    evict();
    await dripper.maintain();
    fees.push(dripper.waiting.get(0n).fee);
  }
  assert.deepEqual(fees, [2n, 4n]);
  evict();
  await assert.rejects(dripper.maintain(), /entry pool is full/);
  assert.equal(dripper.waiting.get(0n).fee, 4n, 'at the cap it waits for room instead');
});

test('requests are checked, and an account waits out its cooldown', async () => {
  let t = 0;
  const node = fakeNode();
  const dripper = new Dripper({ rpc: node, secretKey: SECRET, amount: 100n * KSA, now: () => t });
  await dripper.init();
  const state = {
    dripper,
    cooldown: new Cooldown(24 * 3_600_000, () => t),
    perClient: new RateLimit(5, 5 / 1_440, () => t),
    overall: new RateLimit(120, 2, () => t),
    requests: new RateLimit(30, 30, () => t),
  };
  const post = (to, client = '198.51.100.1') => handle(state, 'POST', client, async () => JSON.stringify({ to }));

  const [status, info] = await handle(state, 'GET', 'x', async () => '');
  assert.equal(status, 200);
  assert.equal(info.chain_id, SIGNET.chain_id);
  assert.match(info.address, /^tb1p/);
  assert.equal(info.amount, (100n * KSA).toString());

  const [ok, sent] = await post(toHex(ALICE));
  assert.equal(ok, 200);
  assert.equal(sent.to, toHex(ALICE));
  assert.equal(sent.nonce, '0');
  await assert.rejects(post(toHex(ALICE), '198.51.100.2'), (e) => e instanceof FaucetError && e.status === 429 && /24 hour/.test(e.message));
  t += 24 * 3_600_000;
  assert.equal((await post(toHex(ALICE)))[0], 200, 'the cooldown is over');

  assert.equal((await post('not an account'))[0], 400);
  assert.equal((await post('bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr'))[0], 400, 'a mainnet address');
  assert.equal((await post(toHex(accountOf(SECRET))))[0], 400, 'the faucet itself');
  assert.equal((await handle(state, 'POST', 'c', async () => '{'))[0], 400);

  for (let i = 0; i < 4; i += 1) {
    assert.equal((await post(toHex(accountOf(new Uint8Array(32).fill(20 + i)))))[0], 200);
  }
  const sixth = toHex(accountOf(new Uint8Array(32).fill(30)));
  let retryAfter;
  await assert.rejects(post(sixth), (e) => {
    retryAfter = e.retryAfter;
    return e.status === 429 && /today/.test(e.message);
  });
  assert.equal(retryAfter, 17_281, 'one request every 288 minutes, and a second past the moment the bucket holds one');
  t += retryAfter * 1000;
  assert.equal((await post(sixth))[0], 200, 'taken once Retry-After has passed');

  node.balance = 0n;
  dripper.balance = 0n;
  const fresh = toHex(accountOf(new Uint8Array(32).fill(40)));
  await assert.rejects(post(fresh, '198.51.100.3'), (e) => e.status === 503);
  assert.equal(state.cooldown.remaining(fresh), 0, 'a failed request does not start the cooldown');
});
