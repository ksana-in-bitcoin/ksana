// KSA faucet of the public signet (D-107): sends KSA from the faucet account of the genesis config to whoever asks,
// as path B transfers signed with the faucet's key and submitted to a seed node. The hosted page reaches it at /faucet:
//
//   GET  /    {"chain_id", "account", "address", "balance", "amount", "cooldown_hours", "waiting"}
//   POST /    {"to": "<signet Taproot address or 64 hex digits>"} → {"op_id", "to", "amount", "nonce"}
//
// Each account gets KSANA_FAUCET_KSA (default 100 KSA) once every KSANA_FAUCET_COOLDOWN_HOURS (default 24 hours); each
// client address (X-Forwarded-For from Caddy) asks 5 times a day; the faucet as a whole sends 120 an hour. The waits
// live in memory, so a restart forgets them. KSANA_FAUCET_CORS names the one other origin whose page may call it, the
// contract playground's (D-119).
//
// The transfers go out one at a time with nonces counted here (the node's account nonce counts only what blocks have
// included), and at most 48 wait for a block at once, below the entry pool's 64 per account (ksana-mempool). Each pays
// the seed nodes' floor fee, or the node's suggested fee up to `MAX_FEE` when the node refuses that fee as below its
// floor or for a full pool (`FEE`). Every 15 seconds the faucet reads its account again: a transfer that dropped out of
// the node's pool (a main-chain switch, an eviction) is submitted again, signed with a higher fee, up to the same cap,
// when the node asks for one.
//
//   KSANA_RPC_URL=http://node1:8545 KSANA_FAUCET_KEY_FILE=/signet/keys/faucet.key PORT=8081 node signet/services/faucet.mjs
import http from 'node:http';
import { readFileSync } from 'node:fs';
import {
  KsanaRpcClient,
  RPC_ERROR_CODES,
  RpcError,
  SIGNET,
  VMOP_VERSION,
  accountOf,
  accountToAddress,
  fromHex,
  ksaAssetId,
  parseAccount,
  signedVmop,
  toHex,
} from '../../sdk/ts/dist/src/index.js';
import { RateLimit, allowOrigin, clientAddress, env, log, readBody, sendJson } from './lib.mjs';

/** Base units per KSA (D-066). */
const KSA_UNIT = 100_000_000n;
/** `max_gas` of a transfer: the gas of the built-in transfer action, the node's reference transfer for fee estimates. */
const TRANSFER_GAS = 10_000n;
/** Transfers waiting for a block at once (the entry pool takes 64 per account). */
const MAX_WAITING = 48;
/** How long a transfer waits before the faucet asks whether the node still has it. */
const RESUBMIT_AFTER_MS = 60_000;
/**
 * The fee of a transfer (base units): the seed nodes' packing floor, `--min-token-fee` 1. The node's suggested fee is
 * the median of the lowest path B fee rates in its last 20 blocks, which on the signet are mostly the faucet's own
 * transfers, so the faucet pays the suggestion only for a transfer the node refuses at this fee: paying twice the
 * suggestion every time raised it with every round of requests.
 */
const FEE = 1n;
/**
 * The most a transfer pays when the node refuses `FEE` (base units): far above any floor the seed nodes set and far
 * below the amount. Without it, others' transfers filling the pool while a few blocks of high fees raise the suggestion
 * would make the faucet pay that suggestion, however high.
 */
const MAX_FEE = 1_000n;

/**
 * Whether a higher fee can get a refused transfer in: the fee is below the node's floor, or its entry pool is full
 * (ksana-mempool's messages). The other refusals are not cured by a higher fee, and one of them is made worse: an op
 * already waiting at the nonce would be replaced, and after a restart that is a transfer the faucet already answered.
 */
function wantsHigherFee(e) {
  return e.code === RPC_ERROR_CODES.OP_REJECTED && /is below this node's minimum|entry pool is full/.test(e.message);
}

/** The faucet cannot take a request now; `status` is the HTTP status, `retryAfter` seconds when known. */
export class FaucetError extends Error {
  constructor(status, message, retryAfter) {
    super(message);
    this.status = status;
    this.retryAfter = retryAfter;
  }
}

/** A KSA amount ("100", "0.5", up to 8 decimals) in base units. */
export function ksaUnits(text) {
  const m = /^(\d{1,12})(?:\.(\d{1,8}))?$/.exec(String(text).trim());
  if (!m) throw new Error(`not a KSA amount: ${text}`);
  const units = BigInt(m[1]) * KSA_UNIT + BigInt((m[2] ?? '').padEnd(8, '0'));
  if (units === 0n) throw new Error('the amount must be above 0');
  return units;
}

/** Base units as KSA, without trailing zeros. */
export function formatKsa(units) {
  const whole = units / KSA_UNIT;
  const frac = (units % KSA_UNIT).toString().padStart(8, '0').replace(/0+$/, '');
  return frac === '' ? whole.toString() : `${whole}.${frac}`;
}

/** One request per key every `ms`: `remaining(key)` is how long a key still waits, `mark(key)` starts its wait. */
export class Cooldown {
  constructor(ms, now = () => Date.now()) {
    this.ms = ms;
    this.now = now;
    this.until = new Map();
  }

  remaining(key) {
    const until = this.until.get(key);
    return until === undefined ? 0 : Math.max(0, until - this.now());
  }

  mark(key) {
    const t = this.now();
    if (this.until.size > 100_000) {
      for (const [k, until] of this.until) if (until <= t) this.until.delete(k);
    }
    this.until.set(key, t + this.ms);
  }

  clear(key) {
    this.until.delete(key);
  }
}

/**
 * The faucet account: signs and submits the transfers one at a time with its own nonce count, and keeps the ones
 * waiting for a block until the account nonce passes them.
 */
export class Dripper {
  constructor({ rpc, secretKey, params = SIGNET, amount, fee = FEE, maxFee = MAX_FEE, now = () => Date.now() }) {
    this.rpc = rpc;
    this.secretKey = secretKey;
    this.params = params;
    this.amount = amount;
    /** The fee of a transfer unless the node refuses it (base units). */
    this.fee = fee;
    /** The most a transfer pays when the node refuses `fee` (base units). */
    this.maxFee = maxFee;
    this.now = now;
    this.account = accountOf(secretKey);
    /** The next nonce to sign. */
    this.next = 0n;
    /** The KSA balance at the last read of the account (base units). */
    this.balance = 0n;
    /** Transfers waiting for a block, by nonce: `{ op, id, to, fee, at }`. */
    this.waiting = new Map();
    this.queue = Promise.resolve();
  }

  /** Checks that the node is on this network and reads the account. */
  async init() {
    const info = await this.rpc.getChainInfo();
    if (info.chain_id !== this.params.chain_id) {
      throw new Error(`the node is on chain ${info.chain_id}, not ${this.params.chain_id}`);
    }
    await this.refresh();
  }

  /** Runs `task` after everything queued before it, so that nonces are handed out in order. */
  serial(task) {
    const run = this.queue.then(task);
    this.queue = run.catch(() => undefined);
    return run;
  }

  /** Sends `amount` KSA to `to` (32 bytes): `{ op_id, nonce, fee }` once the node has taken the transfer. */
  drip(to) {
    return this.serial(() => this.send(to));
  }

  /** Reads the account again and submits the waiting transfers the node lost (every 15 seconds). */
  maintain() {
    return this.serial(() => this.recover());
  }

  sign(nonce, to, fee) {
    return signedVmop(
      {
        version: VMOP_VERSION,
        chain_id: this.params.chain_id,
        account: this.account,
        nonce,
        fee: { kind: 'Token', amount: fee },
        max_gas: TRANSFER_GAS,
        action: { kind: 'AssetTransfer', asset: ksaAssetId(), to, amount: this.amount },
      },
      this.secretKey,
    );
  }

  /** The node's suggested fee for a transfer, at least 1 base unit: asked only when the node refuses a fee. */
  async suggested() {
    const estimate = await this.rpc.estimateFee();
    return estimate.path_b.amount > 0n ? estimate.path_b.amount : 1n;
  }

  /** `fee`, at most `maxFee`. */
  capped(fee) {
    return fee < this.maxFee ? fee : this.maxFee;
  }

  /** The KSA the waiting transfers still take from the balance. */
  committed() {
    let sum = 0n;
    for (const w of this.waiting.values()) sum += this.amount + w.fee;
    return sum;
  }

  /** Whether the balance covers `extra` besides the waiting transfers, reading the account again before saying no. */
  async affords(extra) {
    if (this.balance >= this.committed() + extra) return true;
    await this.refresh();
    return this.balance >= this.committed() + extra;
  }

  async refresh() {
    const account = await this.rpc.getAccount(this.account);
    this.balance = account.balances.find((b) => b.asset.every((x) => x === 0))?.amount ?? 0n;
    for (const nonce of [...this.waiting.keys()]) if (nonce < account.nonce) this.waiting.delete(nonce);
    if (this.next < account.nonce) this.next = account.nonce;
    return account.nonce;
  }

  async send(to) {
    if (this.waiting.size >= MAX_WAITING) {
      throw new FaucetError(503, 'the faucet has many transfers waiting for a block; try again in a minute', 60);
    }
    let fee = this.fee;
    if (!(await this.affords(this.amount + fee))) throw new FaucetError(503, 'the faucet is empty');
    let recounted = false;
    for (;;) {
      const nonce = this.next;
      const op = this.sign(nonce, to, fee);
      try {
        const { op_id } = await this.rpc.submitOp(op);
        this.waiting.set(nonce, { op, id: op_id, to, fee, at: this.now() });
        this.next = nonce + 1n;
        return { op_id, nonce, fee };
      } catch (e) {
        if (!(e instanceof RpcError)) throw e;
        // Someone else signed with the faucet's key, or a block took the nonce: count again from the account
        if (!recounted && /already used/.test(e.message)) {
          recounted = true;
          await this.refresh();
          continue;
        }
        // The node's floor is above the faucet's fee, or its pool is full: pay its suggestion for this transfer, up to
        // the cap, if the balance still covers the transfer at that fee
        if (fee === this.fee && wantsHigherFee(e)) {
          const raised = this.capped(await this.suggested());
          if (raised > fee) {
            if (!(await this.affords(this.amount + raised))) throw new FaucetError(503, 'the faucet is empty');
            fee = raised;
            continue;
          }
        }
        throw e;
      }
    }
  }

  async recover() {
    const nonce = await this.refresh();
    const first = this.waiting.get(nonce);
    if (first === undefined || this.now() - first.at < RESUBMIT_AFTER_MS) return;
    let lost = false;
    try {
      lost = (await this.rpc.getOpStatus(first.id)).status === 'skipped';
    } catch (e) {
      if (!(e instanceof RpcError && e.code === RPC_ERROR_CODES.NOT_FOUND)) throw e;
      lost = true;
    }
    if (!lost) return;
    log('faucet', `the transfer with nonce ${nonce} left the node's pool; submitting the ${this.waiting.size} waiting again`);
    const order = [...this.waiting.keys()].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    for (const n of order) {
      const w = this.waiting.get(n);
      try {
        await this.rpc.submitOp(w.op);
        w.at = this.now();
      } catch (e) {
        if (!(e instanceof RpcError && wantsHigherFee(e))) throw e;
        // The node wants a higher fee (its floor rose, or the pool is full): sign the same transfer again, up to the cap;
        // at the cap it waits for a later round
        const fee = this.capped([w.fee * 2n, await this.suggested()].reduce((a, b) => (a > b ? a : b)));
        if (fee <= w.fee) throw e;
        if (this.balance < this.committed() + fee - w.fee) throw new FaucetError(503, 'the faucet is empty');
        const op = this.sign(n, w.to, fee);
        const { op_id } = await this.rpc.submitOp(op);
        Object.assign(w, { op, id: op_id, fee, at: this.now() });
      }
    }
  }
}

/** Handles one request: `[status, body, headers]`. */
export async function handle({ dripper, cooldown, perClient, overall, requests }, method, client, body) {
  if (method === 'GET') {
    return [
      200,
      {
        chain_id: dripper.params.chain_id,
        account: toHex(dripper.account),
        address: accountToAddress(dripper.account, 'signet'),
        balance: dripper.balance.toString(),
        amount: dripper.amount.toString(),
        cooldown_hours: cooldown.ms / 3_600_000,
        waiting: dripper.waiting.size,
      },
    ];
  }
  if (method !== 'POST') return [405, { error: 'GET the faucet, or POST {"to": "<account>"}' }];
  if (!requests.take(client)) throw new FaucetError(429, 'too many requests from this address; slow down', 60);
  let request;
  try {
    request = JSON.parse(await body());
  } catch {
    return [400, { error: 'send {"to": "<signet Taproot address or 64 hex digits>"}' }];
  }
  let to;
  try {
    to = parseAccount(String(request?.to ?? ''), 'signet');
  } catch (e) {
    return [400, { error: e.message }];
  }
  const key = toHex(to);
  if (key === toHex(dripper.account)) return [400, { error: 'that is the faucet itself' }];
  const wait = cooldown.remaining(key);
  if (wait > 0) {
    const hours = Math.ceil(wait / 3_600_000);
    throw new FaucetError(429, `this account already received KSA; it can ask again in ${hours} hour(s)`, Math.ceil(wait / 1000));
  }
  if (!perClient.take(client)) {
    // the bucket refills one request every 288 minutes (5 a day): Retry-After is when the next one is there
    throw new FaucetError(429, 'this address has asked too often today', Math.ceil(perClient.wait(client) / 1000));
  }
  if (!overall.take('faucet')) throw new FaucetError(429, 'the faucet is busy; try again in a few minutes', 300);
  cooldown.mark(key);
  try {
    const { op_id, nonce } = await dripper.drip(to);
    log('faucet', `${client} → ${key}: ${formatKsa(dripper.amount)} KSA, nonce ${nonce}, op ${toHex(op_id)}`);
    return [200, { op_id: toHex(op_id), to: key, amount: dripper.amount.toString(), nonce: nonce.toString() }];
  } catch (e) {
    cooldown.clear(key);
    throw e;
  }
}

async function serve() {
  const port = Number(env('PORT', '8081'));
  const rpcUrl = env('KSANA_RPC_URL', 'http://node1:8545');
  const keyFile = env('KSANA_FAUCET_KEY_FILE', '/signet/keys/faucet.key');
  const amount = ksaUnits(env('KSANA_FAUCET_KSA', '100'));
  const cooldownHours = Number(env('KSANA_FAUCET_COOLDOWN_HOURS', '24'));
  const secretKey = fromHex(readFileSync(keyFile, 'utf8').trim(), 32, 'faucet key');
  const dripper = new Dripper({ rpc: new KsanaRpcClient(rpcUrl, { timeoutMs: 30_000 }), secretKey, amount });
  for (;;) {
    try {
      await dripper.init();
      break;
    } catch (e) {
      log('faucet', `waiting for the node at ${rpcUrl}: ${e.message}`);
      await new Promise((r) => setTimeout(r, 10_000));
    }
  }
  log('faucet', `account ${accountToAddress(dripper.account, 'signet')}, ${formatKsa(dripper.balance)} KSA, ${formatKsa(amount)} KSA per request`);
  setInterval(() => {
    dripper.maintain().catch((e) => log('faucet', `reading the account: ${e.message}`));
  }, 15_000);

  const state = {
    dripper,
    cooldown: new Cooldown(cooldownHours * 3_600_000),
    perClient: new RateLimit(5, 5 / 1_440),
    overall: new RateLimit(120, 2),
    requests: new RateLimit(30, 30),
  };
  const server = http.createServer(allowOrigin(process.env.KSANA_FAUCET_CORS ?? '', async (req, res) => {
    const client = clientAddress(req);
    try {
      const [status, body] = await handle(state, req.method, client, () => readBody(req, 4_096));
      sendJson(res, status, body);
    } catch (e) {
      if (e instanceof FaucetError) {
        if (e.retryAfter !== undefined) res.setHeader('retry-after', String(e.retryAfter));
        sendJson(res, e.status, { error: e.message });
      } else if (e?.status === 413) {
        sendJson(res, 413, { error: e.message });
      } else if (e instanceof RpcError) {
        log('faucet', `${client}: the node refused the transfer: ${e.message}`);
        sendJson(res, 502, { error: `the node refused the transfer: ${e.message}` });
      } else {
        log('faucet', `${client}: ${e?.message ?? e}`);
        sendJson(res, 502, { error: 'the node is unavailable' });
      }
    }
  }));
  server.listen(port, () => log('faucet', `listening on ${port}`));
}

if (import.meta.url === `file://${process.argv[1]}`) await serve();
