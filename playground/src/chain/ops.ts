/**
 * Wallet-signed path B operations: deployments, calls and asset transfers (D-118's journal, carried over from the web page's
 * first playground). No private keys, BTC transactions or automatic retries.
 *
 * - Every operation is reviewed before the wallet signs it, and checked again (account, node, network, nonce, fee, balances)
 *   both before and after signing.
 * - A write-ahead journal in localStorage holds the operation's ID before it is broadcast, so a lost answer never leads to a second
 *   operation at the same nonce: the nonce stays blocked until the node shows the operation, or the user releases it explicitly
 *   after the node reports it unknown at the account's current nonce. No key, signature, argument or code is stored.
 * - Writes for one chain and account take a Web Lock, so two tabs cannot sign at the same nonce.
 */
import {
  DEVNET, SIGNET, VMOP_VERSION, RPC_ERROR_CODES, RpcError,
  decodeAddress, encodeVmOp, feeFor, fromHex, opCost, opId, sha256, signVmopWithWallet, toHex,
  type Action, type BtcNetwork, type Funds, type KsanaRpcClient, type OpStatus, type Params, type UnsignedVmOp, type VmOp,
  type WalletAccount, type WalletAdapter,
} from '@ksana/sdk';

export type Problem = 'hex' | 'contract' | 'method' | 'gas' | 'wasm' | 'wasmSize' | 'wasmAbi' | 'funds' |
  'changed' | 'nonce' | 'balance' | 'pending' | 'storage' | 'busy' | 'locks' | 'feeChanged' | 'opId' | 'network' | 'release' |
  'walletNetwork';

export class PlaygroundError extends Error {
  constructor(readonly problem: Problem, readonly detail = '') {
    super(detail ? `${problem}: ${detail}` : problem);
    this.name = 'PlaygroundError';
  }
}

function fail(problem: Problem, detail = ''): never {
  throw new PlaygroundError(problem, detail);
}

/** The gas a deployment gets by default: its code is charged 21 per byte (20 to store, 1 to load), `init` runs within the rest */
export const DEFAULT_GAS = 1_000_000n;
/** A deployment's code must fit in one operation (`MAX_OP_BYTES`, 64 KiB, with the rest of the operation) */
export const MAX_WASM_BYTES = 64 * 1024;
/** The most funds one operation attaches (ABI §2 item 7) */
export const MAX_CALL_FUNDS = 16;

export const KSA = new Uint8Array(32);

export function isKsa(asset: Uint8Array): boolean {
  return asset.length === 32 && asset.every((b) => b === 0);
}

export function deployGas(codeBytes: number): bigint {
  return DEFAULT_GAS + BigInt(codeBytes) * 21n;
}

/** Gas for a call from a read-only run of it: half again what it used, and room for the state having moved on */
export function callGas(used: bigint, params: Params): bigint {
  const gas = used + used / 2n + 20_000n;
  return gas > params.block_gas_limit ? params.block_gas_limit : gas;
}

export function contractIdOf(deployer: Uint8Array, nonce: bigint, codeHash: Uint8Array): string {
  const preimage = new Uint8Array(72);
  preimage.set(deployer, 0);
  new DataView(preimage.buffer).setBigUint64(32, nonce, true);
  preimage.set(codeHash, 40);
  return toHex(sha256(preimage));
}

/** Attached funds as the protocol takes them: at most 16 items, asset IDs strictly ascending, every amount above 0 */
export function sortFunds(funds: readonly Funds[]): Funds[] {
  const merged = new Map<string, bigint>();
  for (const f of funds) {
    if (f.asset.length !== 32 || f.amount <= 0n || f.amount >= 1n << 128n) fail('funds');
    const key = toHex(f.asset);
    merged.set(key, (merged.get(key) ?? 0n) + f.amount);
  }
  const out = [...merged.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)).map(([asset, amount]) => {
    if (amount >= 1n << 128n) fail('funds');
    return { asset: fromHex(asset, 32), amount };
  });
  if (out.length > MAX_CALL_FUNDS) fail('funds');
  return out;
}

export function checkFunds(funds: readonly Funds[]): void {
  if (funds.length > MAX_CALL_FUNDS) fail('funds');
  for (let i = 0; i < funds.length; i++) {
    const f = funds[i]!;
    if (f.asset.length !== 32 || f.amount <= 0n || f.amount >= 1n << 128n) fail('funds');
    if (i > 0 && toHex(funds[i - 1]!.asset) >= toHex(f.asset)) fail('funds');
  }
}

/** Structural preflight only: the node remains authoritative for Ksana validation and gas. Never executes the module. */
export async function validateWasm(bytes: Uint8Array): Promise<void> {
  if (!bytes.length || bytes.length > MAX_WASM_BYTES) fail('wasmSize');
  let module: WebAssembly.Module;
  const copy = Uint8Array.from(bytes);
  try {
    if (!WebAssembly.validate(copy)) return fail('wasm');
    module = await WebAssembly.compile(copy);
  } catch {
    return fail('wasm');
  }
  const exports = WebAssembly.Module.exports(module);
  if (![['memory', 'memory'], ['ksana_alloc', 'function'], ['ksana_call', 'function']].every(([name, kind]) =>
    exports.some((e) => e.name === name && e.kind === kind)) ||
    WebAssembly.Module.imports(module).some((i) => i.module !== 'ksana' || i.kind !== 'function')) fail('wasmAbi');
}

export type EntryKind = 'Deploy' | 'Call' | 'AssetTransfer';
export type EntryState = 'submitting' | 'accepted' | 'unknown' | 'rejected' | 'included' | 'released' | 'retired';
const KINDS: readonly string[] = ['Deploy', 'Call', 'AssetTransfer'];
const STATES: readonly string[] = ['submitting', 'accepted', 'unknown', 'rejected', 'included', 'released', 'retired'];

export interface Entry {
  endpoint: string;
  chainId: number;
  account: string;
  nonce: string;
  opId: string;
  /** The contract deployed or called, or the asset transferred */
  contract: string;
  kind: EntryKind;
  /** The method called (a Call only) */
  method?: string;
  at: number;
  state: EntryState;
}

export const JOURNAL_KEY = 'ksana:playground:v1';
const MAX_ROWS = 64;

/**
 * No key, signature, argument or WASM bytes are persisted. Corrupt/unwritable storage fails closed before broadcast. One journal per
 * chain and account, the chain named by its genesis block (`genesis`, hex) as well as its ID: a network started again from genesis
 * keeps its chain ID, and its accounts' nonces start over, which the rows of the chain before would block for good.
 */
export class Journal {
  readonly key: string;

  constructor(private readonly storage: Pick<Storage, 'getItem' | 'setItem'>, readonly chainId: number, readonly genesis: string,
    readonly account: string) {
    this.key = `${JOURNAL_KEY}:${chainId}:${genesis}:${account}`;
  }

  read(): Entry[] {
    try {
      const rows: unknown = JSON.parse(this.storage.getItem(this.key) ?? '[]');
      if (!Array.isArray(rows) || rows.length > MAX_ROWS || !rows.every((r: Entry) => r &&
        [r.endpoint, r.account, r.opId, r.contract].every((v) => typeof v === 'string' && /^[0-9a-f]{64}$/.test(v)) &&
        r.chainId === this.chainId && r.account === this.account && typeof r.nonce === 'string' && /^(0|[1-9][0-9]*)$/.test(r.nonce) &&
        BigInt(r.nonce) < 1n << 64n && Number.isSafeInteger(r.at) &&
        (r.method === undefined || (typeof r.method === 'string' && r.method.length <= 128)) &&
        KINDS.includes(r.kind) && STATES.includes(r.state))) fail('storage');
      return rows;
    } catch {
      return fail('storage');
    }
  }

  put(entry: Entry): void {
    try {
      if (entry.chainId !== this.chainId || entry.account !== this.account) fail('storage');
      let rows = [...this.read().filter((r) => r.opId !== entry.opId), entry];
      while (rows.length > MAX_ROWS) {
        const i = rows.findIndex((r) => r.state === 'included' || r.state === 'rejected' || r.state === 'retired');
        if (i < 0) fail('storage');
        rows = rows.filter((_, j) => j !== i);
      }
      this.storage.setItem(this.key, JSON.stringify(rows));
    } catch {
      fail('storage');
    }
  }

  pending(nonce: bigint): boolean {
    // Retired rows block again if the account nonce goes backwards; an explicit release is different.
    return this.read().some((r) => r.state !== 'rejected' && r.state !== 'released' && BigInt(r.nonce) >= nonce);
  }

  /** Call under the chain/account Web Lock. Nonce advancement is not this op's execution receipt. */
  retireBefore(nonce: bigint): void {
    const rows = this.read();
    let changed = false;
    const next = rows.map((row): Entry => {
      if (BigInt(row.nonce) >= nonce || ['included', 'rejected', 'retired'].includes(row.state)) return row;
      changed = true;
      return { ...row, state: 'retired' };
    });
    if (changed) {
      try {
        this.storage.setItem(this.key, JSON.stringify(next));
      } catch {
        fail('storage');
      }
    }
  }

  /** Merge a status into the latest row, never resurrect an evicted row or undo a local release with a pending report. */
  observe(opId: string, state: 'accepted' | 'included'): void {
    const row = this.read().find((entry) => entry.opId === opId);
    if (!row || (state === 'accepted' && ['released', 'retired'].includes(row.state))) return;
    if (row.state !== state) this.put({ ...row, state });
  }
}

export function shouldPoll(status: OpStatus['status'] | undefined, attempts: number, failures: number): boolean {
  return status !== 'btc_final' && attempts < 120 && failures < 3;
}

export function endpointKey(url: string, chainId: number): string {
  // A custom endpoint might contain credentials. Store only its fingerprint, never its URL.
  return toHex(sha256(new TextEncoder().encode(`${chainId}:${url}`)));
}

export interface SigningWallet {
  name: string;
  adapter: WalletAdapter;
  account: WalletAccount;
}

export interface Context {
  node: KsanaRpcClient;
  params: Params;
  /** The node's genesis block (hex) */
  genesis: string;
  network: BtcNetwork;
  endpoint: string;
  wallet: SigningWallet;
  journal: Journal;
  isCurrent(): boolean;
  /** Operations this page knows to be waiting in the node's pool */
  pendingOperations(): { opId: string; nonce: bigint }[];
  onSubmitted(op: VmOp, id: string): void;
}

export interface Prepared {
  unsigned: UnsignedVmOp;
  contract: string;
  fee: bigint;
  endpoint: string;
}

/** All journal updates for one chain/account share this lock, even when tabs use different RPC endpoints. */
export async function withWriteLock<T>(journal: Journal, fn: () => Promise<T>): Promise<T> {
  if (typeof navigator === 'undefined' || !navigator.locks) return fail('locks');
  return navigator.locks.request(journal.key, { ifAvailable: true }, (lock) => {
    if (!lock) return fail('busy');
    return fn();
  });
}

/** The BTC network the wallet says it is set to must be the node's (a tb1 address alone does not tell signet from testnet) */
export async function checkWalletNetwork(adapter: WalletAdapter, network: BtcNetwork): Promise<boolean> {
  const told = await adapter.getNetwork?.();
  if (!told) return false;
  if (told.network !== network) fail('walletNetwork', told.name);
  return true;
}

async function checkContext(c: Context): Promise<void> {
  if (![DEVNET.chain_id, SIGNET.chain_id].includes(c.params.chain_id)) fail('network');
  if (!c.isCurrent()) fail('changed');
  if (c.journal.chainId !== c.params.chain_id || c.journal.genesis !== c.genesis || c.journal.account !== c.wallet.account.account) fail('changed');
  await c.node.getParams(c.params);
  await checkWalletNetwork(c.wallet.adapter, c.network);
  const account = await c.wallet.adapter.getAccount();
  if (!c.isCurrent() || account.account.toLowerCase() !== c.wallet.account.account.toLowerCase() ||
    !decodeAddress(account.address).networks.includes(c.network)) fail('changed');
}

function encodedSize(unsigned: UnsignedVmOp): number {
  return encodeVmOp({ ...unsigned, sig: { kind: 'Bip322Simple', bytes: new Uint8Array(67) } }).length;
}

/** What the operation takes from the account, per asset (hex): the fee in KSA, the attached funds or the transfer */
export function amountsNeeded(action: Action, fee: bigint): Map<string, bigint> {
  const need = new Map<string, bigint>([[toHex(KSA), fee]]);
  const add = (asset: Uint8Array, amount: bigint) => need.set(toHex(asset), (need.get(toHex(asset)) ?? 0n) + amount);
  if (action.kind === 'AssetTransfer') add(action.asset, action.amount);
  else action.funds.forEach((f) => add(f.asset, f.amount));
  return need;
}

function checkBalances(balances: { asset: Uint8Array; amount: bigint }[], need: Map<string, bigint>): void {
  for (const [asset, amount] of need) {
    const have = balances.find((b) => toHex(b.asset) === asset)?.amount ?? 0n;
    if (have < amount) fail('balance', asset);
  }
}

async function fresh(c: Context, unsigned?: UnsignedVmOp) {
  await checkContext(c);
  const [account, estimate] = await Promise.all([c.node.getAccount(c.wallet.account.account), c.node.estimateFee()]);
  if (!c.isCurrent()) fail('changed');
  if (unsigned && account.nonce !== unsigned.nonce) fail('nonce');
  c.journal.retireBefore(account.nonce);
  const released = new Set(c.journal.read().filter((r) => r.state === 'released').map((r) => r.opId));
  // A released op can remain in the page's stale pool cache. Never ignore an unrelated op at the same nonce.
  if (c.pendingOperations().some((op) => op.nonce >= account.nonce && !released.has(op.opId)) || c.journal.pending(account.nonce)) fail('pending');
  return { account, estimate };
}

function snapshotAction(action: Action): Action {
  const snapshot = structuredClone(action);
  if (snapshot.kind === 'AssetTransfer') {
    if (snapshot.asset.length !== 32 || snapshot.to.length !== 32 || snapshot.amount <= 0n || snapshot.amount >= 1n << 128n) fail('funds');
  } else {
    checkFunds(snapshot.funds);
  }
  if (snapshot.kind === 'Call' && (!snapshot.method || new TextEncoder().encode(snapshot.method).length > 128)) fail('method');
  return snapshot;
}

export async function prepare(c: Context, action: Action, maxGas: bigint): Promise<Prepared> {
  const snapshot = snapshotAction(action);
  if (snapshot.kind === 'Deploy') await validateWasm(snapshot.code);
  if (maxGas < 1n || maxGas > c.params.block_gas_limit) fail('gas');
  const { account, estimate } = await fresh(c);
  const unsigned: UnsignedVmOp = {
    version: VMOP_VERSION, chain_id: c.params.chain_id, account: fromHex(c.wallet.account.account, 32),
    nonce: account.nonce, max_gas: maxGas, action: snapshot, fee: { kind: 'Token', amount: 0n },
  };
  const size = encodedSize(unsigned);
  if (size > c.params.max_op_bytes) fail('wasmSize');
  const fee = feeFor(estimate.path_b.per_kib, opCost(c.params, size, maxGas), estimate.path_b.amount);
  unsigned.fee = { kind: 'Token', amount: fee };
  checkBalances(account.balances, amountsNeeded(snapshot, fee));
  const contract = snapshot.kind === 'Deploy' ? contractIdOf(unsigned.account, unsigned.nonce, sha256(snapshot.code)) :
    snapshot.kind === 'Call' ? toHex(snapshot.contract) : toHex(snapshot.asset);
  return { unsigned, contract, fee, endpoint: c.endpoint };
}

async function recheck(c: Context, p: Prepared): Promise<void> {
  if (p.endpoint !== c.endpoint || p.unsigned.chain_id !== c.params.chain_id ||
    toHex(p.unsigned.account) !== c.wallet.account.account || p.unsigned.fee.kind !== 'Token' || p.unsigned.fee.amount !== p.fee) fail('changed');
  const { account, estimate } = await fresh(c, p.unsigned);
  const required = feeFor(estimate.path_b.per_kib, opCost(c.params, encodedSize(p.unsigned), p.unsigned.max_gas), estimate.path_b.amount);
  if (required > p.fee) fail('feeChanged');
  checkBalances(account.balances, amountsNeeded(p.unsigned.action, p.fee));
}

const busy = new Set<string>();

/** Explicit local recovery only: no signing, broadcast, cancellation, or claim that the old operation failed. */
export async function releaseNonce(c: Context, id: string, confirmed = false): Promise<Entry> {
  if (!confirmed) fail('release');
  return withWriteLock(c.journal, async () => {
    if (busy.has(`${c.params.chain_id}:${c.wallet.account.account}`)) fail('busy');
    await checkContext(c);
    const row = c.journal.read().find((entry) => entry.opId === id);
    if (!row || ['rejected', 'released'].includes(row.state)) fail('release');
    let missing = false;
    try {
      await c.node.getOpStatus(id);
    } catch (error) {
      if (error instanceof RpcError && error.code === RPC_ERROR_CODES.NOT_FOUND) missing = true;
      else throw error;
    }
    if (!missing) fail('pending');
    const account = await c.node.getAccount(c.wallet.account.account);
    await checkContext(c);
    if (account.nonce !== BigInt(row.nonce)) fail('nonce');
    const latest = c.journal.read().find((entry) => entry.opId === id);
    if (!latest || latest.state !== row.state || latest.nonce !== row.nonce) fail('changed');
    const released: Entry = { ...latest, state: 'released' };
    c.journal.put(released);
    return released;
  });
}

/** Call under a Web Lock as well in the UI. This synchronous guard also prevents repeated calls in the same tab. */
export async function submit(c: Context, p: Prepared): Promise<{ entry: Entry; error?: unknown }> {
  const key = `${c.params.chain_id}:${c.wallet.account.account}`;
  if (busy.has(key)) fail('busy');
  busy.add(key);
  try {
    await recheck(c, p);
    const signed = await signVmopWithWallet(p.unsigned, c.wallet.adapter);
    await recheck(c, p);
    if (encodeVmOp(signed).length > c.params.max_op_bytes || encodeVmOp(signed).length > encodedSize(p.unsigned)) fail('wasmSize');
    const id = toHex(opId(signed));
    const entry: Entry = {
      endpoint: c.endpoint, chainId: c.params.chain_id, account: c.wallet.account.account, nonce: signed.nonce.toString(),
      opId: id, contract: p.contract, kind: signed.action.kind, at: Date.now(), state: 'submitting',
      ...(signed.action.kind === 'Call' ? { method: signed.action.method } : {}),
    };
    c.journal.put(entry); // Must succeed before any broadcast; never store signed bytes.
    try {
      const sent = await c.node.submitOp(signed);
      if (toHex(sent.op_id) !== id) fail('opId');
    } catch (error) {
      const rejected = error instanceof RpcError && new Set<number>([RPC_ERROR_CODES.INVALID_PARAMS, RPC_ERROR_CODES.OP_INVALID,
        RPC_ERROR_CODES.OP_REJECTED, RPC_ERROR_CODES.METHOD_NOT_FOUND]).has(error.code);
      const failed: Entry = { ...entry, state: rejected ? 'rejected' : 'unknown' };
      try {
        c.journal.put(failed);
      } catch {
        /* The durable submitting marker still holds the operation ID. */
      }
      return { entry: failed, error };
    }
    const accepted: Entry = { ...entry, state: 'accepted' };
    try {
      c.journal.put(accepted);
      if (c.isCurrent()) c.onSubmitted(signed, id);
    } catch (error) {
      return { entry: accepted, error };
    }
    return { entry: accepted };
  } finally {
    busy.delete(key);
  }
}
