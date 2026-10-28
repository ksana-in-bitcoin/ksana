/**
 * The token-swap example (`examples/token-swap`) against a fake node: `add` attaches what its quote says the pool takes of the
 * amounts given, and refuses before signing a quote that takes more than an amount given, or none of it. The compiled example
 * runs as a child process; an HTTP server in this process answers its JSON-RPC calls as a node on signet would.
 */

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { SIGNET, type VmOp, concatBytes, decodeVmOp, fromHex, ksaAssetId, opId, toHex, utf8 } from '../src/index.js';

/** The compiled example, beside this compiled test (`dist/test/` and `dist/examples/`). */
const CLI = fileURLToPath(new URL('../examples/token-swap/main.js', import.meta.url));
const COIN = 100_000_000n;
/** The pool's side b, a token of 8 decimals; side a is KSA. */
const TOKEN = fromHex('5a'.repeat(32));
const POOL = '9c'.repeat(32);
const BLOCK = { height: '12', hash: 'ab'.repeat(32) };

const keyDir = mkdtempSync(join(tmpdir(), 'ksana-token-swap-'));
const keyFile = join(keyDir, 'test.key');
writeFileSync(keyFile, `${'11'.repeat(32)}\n`, { mode: 0o600 });
after(() => rmSync(keyDir, { recursive: true, force: true }));

function u128(value: bigint): Uint8Array {
  const out = new Uint8Array(16);
  const view = new DataView(out.buffer);
  view.setBigUint64(0, value & 0xffff_ffff_ffff_ffffn, true);
  view.setBigUint64(8, value >> 64n, true);
  return out;
}

function borshString(text: string): Uint8Array {
  const bytes = utf8(text);
  const len = new Uint8Array(4);
  new DataView(len.buffer).setUint32(0, bytes.length, true);
  return concatBytes(len, bytes);
}

function receipt(output: Uint8Array, events: { contract: string; data: string }[] = []) {
  return { op_id: null, success: true, error: null, gas_used: '1000', output: toHex(output), events };
}

/** The deposit the fake node's quote of `add_liquidity` reports: the amounts the pool takes, and the shares. */
interface Quote {
  taken: [bigint, bigint];
  shares: bigint;
}

interface FakeNode {
  url: string;
  /** The methods called, in order. */
  methods: string[];
  /** The ops submitted. */
  submitted: VmOp[];
  close(): Promise<void>;
}

/** A node on signet with a pool of KSA and the token, whose quotes of `add_liquidity` report `quote`. */
async function fakeNode(quote: Quote): Promise<FakeNode> {
  const methods: string[] = [];
  const submitted: VmOp[] = [];
  const answer = (method: string, params: any[]): unknown => {
    switch (method) {
      case 'ksana_getChainInfo':
        return { chain_id: SIGNET.chain_id, tip: BLOCK, daa_score: '120', protocol_version: 2, next_upgrade: null, max_known_version: 2, finalized: BLOCK };
      case 'ksana_getAsset':
        return {
          asset: params[0],
          name: 'Wizards',
          symbol: 'WIZ',
          decimals: 8,
          total_supply: `${21_000_000n * COIN}`,
          issuer: '77'.repeat(32),
          capabilities: [],
          paused: false,
          at: BLOCK,
        };
      case 'ksana_getAccount':
        return { account: params[0], nonce: '5', balances: [], locked_fees: [], at: BLOCK };
      case 'ksana_estimateFee':
        return { feerate_sat_vb: '2', path_b: { amount: '1000', per_kib: '0' }, at: BLOCK };
      case 'ksana_call': {
        const { method: called, caller } = params[0];
        if (called === 'tokens') return receipt(concatBytes(Uint8Array.of(1), ksaAssetId(), Uint8Array.of(1), TOKEN));
        if (called === 'reserves') return receipt(concatBytes(u128(20n * COIN), u128(10n * COIN)));
        if (called === 'add_liquidity') {
          // ("add_liquidity", provider, amount_a, amount_b, shares)
          const data = concatBytes(borshString('add_liquidity'), fromHex(caller), u128(quote.taken[0]), u128(quote.taken[1]), u128(quote.shares));
          return receipt(u128(quote.shares), [{ contract: POOL, data: toHex(data) }]);
        }
        break;
      }
      case 'ksana_submitOp': {
        const op = decodeVmOp(fromHex(params[0]), SIGNET);
        submitted.push(op);
        return { op_id: toHex(opId(op)) };
      }
      case 'ksana_getOpStatus':
        return { status: 'fast_confirmed', block: BLOCK, index: 0, receipt: receipt(u128(quote.shares)) };
    }
    throw new Error(`the fake node has no answer to ${method}`);
  };
  const server = createServer((req, res) => {
    let body = '';
    req.setEncoding('utf8').on('data', (chunk: string) => (body += chunk));
    req.on('end', () => {
      const { id, method, params } = JSON.parse(body);
      methods.push(method);
      let reply: object;
      try {
        reply = { jsonrpc: '2.0', id, result: answer(method, params) };
      } catch (e) {
        reply = { jsonrpc: '2.0', id, error: { code: -32601, message: e instanceof Error ? e.message : String(e) } };
      }
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify(reply));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address() as AddressInfo;
  const close = () =>
    new Promise<void>((resolve) => {
      server.closeAllConnections();
      server.close(() => resolve());
    });
  return { url: `http://127.0.0.1:${port}`, methods, submitted, close };
}

/** `token-swap add --a 10 --b 10` against `node`: its exit status and output. */
function add(node: FakeNode): Promise<{ status: number | null; stdout: string; stderr: string }> {
  const args = ['add', '--key', keyFile, '--pool', POOL, '--a', '10', '--b', '10', '--rpc', node.url, '--chain', 'signet'];
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [CLI, ...args], { stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8').on('data', (chunk: string) => (stdout += chunk));
    child.stderr.setEncoding('utf8').on('data', (chunk: string) => (stderr += chunk));
    child.on('error', reject);
    child.on('close', (status) => resolve({ status, stdout, stderr }));
  });
}

test('token-swap add attaches what the quote takes of the amounts given, with a minimum 1% below its shares', { timeout: 60_000 }, async () => {
  // The pool's ratio, 2 KSA to 1 token, takes all of the 10 KSA and half of the 10 tokens
  const node = await fakeNode({ taken: [10n * COIN, 5n * COIN], shares: 7n * COIN });
  try {
    const run = await add(node);
    assert.equal(run.status, 0, run.stderr);
    assert.match(run.stdout, /deposit +10 KSA \+ 5 Wizards \(5a5a5a5a…\), what the pool's ratio takes/);
    assert.equal(node.submitted.length, 1);
    const action = node.submitted[0]?.action;
    assert.ok(action?.kind === 'Call', 'the op calls the pool');
    assert.equal(action.method, 'add_liquidity');
    assert.equal(toHex(action.args), toHex(concatBytes(u128(10n * COIN), u128(5n * COIN), u128(693_000_000n))));
    assert.deepEqual(
      action.funds.map((f) => [toHex(f.asset), f.amount]),
      [
        [toHex(ksaAssetId()), 10n * COIN],
        [toHex(TOKEN), 5n * COIN],
      ],
    );
  } finally {
    await node.close();
  }
});

const REFUSED: { name: string; taken: [bigint, bigint]; error: string }[] = [
  // The review of M33: a node answering 50 times the amounts given got an op attaching them signed and submitted
  { name: 'more than given of both', taken: [500n * COIN, 250n * COIN], error: 'the quote says the pool takes 500 KSA, more than the 10 KSA given' },
  {
    name: 'one base unit more than given of one',
    taken: [10n * COIN, 10n * COIN + 1n],
    error: 'the quote says the pool takes 10.00000001 Wizards (5a5a5a5a…), more than the 10 Wizards (5a5a5a5a…) given',
  },
  { name: 'none of one', taken: [0n, 5n * COIN], error: 'the quote says the pool takes none of the 10 KSA given' },
];

for (const { name, taken, error } of REFUSED) {
  test(`token-swap add refuses a quote taking ${name}, before signing`, { timeout: 60_000 }, async () => {
    const node = await fakeNode({ taken, shares: 7n * COIN });
    try {
      const run = await add(node);
      assert.equal(run.status, 1, run.stdout);
      assert.ok(run.stderr.includes(error), run.stderr);
      assert.ok(run.stderr.includes('so nothing was signed'), run.stderr);
      assert.deepEqual(node.submitted, []);
      // Refused at the quote: not even the nonce or the fee of an op was asked for
      for (const method of ['ksana_getAccount', 'ksana_estimateFee', 'ksana_submitOp']) assert.ok(!node.methods.includes(method), method);
    } finally {
      await node.close();
    }
  });
}
