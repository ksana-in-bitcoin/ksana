/**
 * Running an operation from any page: a read-only run of a call first (its gas, and whether it would fail now), then the review
 * the person signs from (src/ui/Review.tsx), then the submission through the journal (ops.ts), then its status until a block
 * includes it. `runOp` resolves with the receipt, or rejects when the person cancels or the node refuses the operation.
 */
import type { Action, OpStatus, Receipt, VmOp } from '@ksana/sdk';
import { createStore, useStore } from '../app/store';
import { refreshAccount } from './account';
import { connectionStore, currentConnection, type Connection } from './connection';
import {
  DEFAULT_GAS, Journal, PlaygroundError, callGas, deployGas, prepare, releaseNonce, submit, withWriteLock,
  type Context, type Entry, type Prepared,
} from './ops';
import { walletStore, type ConnectedWallet } from './wallet';

export interface Detail {
  label: string;
  value: string;
}

export interface OpRequest {
  /** What the person is doing, in their words: "Swap 1 KSA for PLAY" */
  title: string;
  action: Action;
  /** The gas limit; by default a deployment's from its size, a call's from a read-only run of it */
  gas?: bigint;
  /** The arguments as people read them */
  details?: Detail[];
}

export type ReviewPhase = 'preparing' | 'ready' | 'signing' | 'failed';

export interface Review {
  id: number;
  request: OpRequest;
  phase: ReviewPhase;
  prepared?: Prepared;
  context?: Context;
  /** A call's read-only run against the current state */
  simulation?: Receipt;
  error?: unknown;
}

export interface Activity {
  opId: string;
  title: string;
  entry: Entry;
  status: OpStatus | null;
  /** The person was told the node could not find it */
  missing?: boolean;
  error?: unknown;
}

export class CancelledError extends Error {
  constructor() {
    super('cancelled');
    this.name = 'CancelledError';
  }
}

export class OpFailedError extends Error {
  constructor(readonly activity: Activity) {
    super(activity.status?.receipt?.error ?? 'operation failed');
    this.name = 'OpFailedError';
  }
}

export const reviewStore = createStore<Review | null>(null);
export const activityStore = createStore<Activity[]>([]);

let reviewId = 0;
let pendingReview: { id: number; resolve: (a: Activity) => void; reject: (e: unknown) => void } | null = null;

function journalFor(conn: Connection, wallet: ConnectedWallet): Journal {
  return new Journal(window.localStorage, conn.params.chain_id, conn.genesis, wallet.account.account);
}

/** The journal of the current node and wallet, or null without one (or without storage) */
export function currentJournal(): Journal | null {
  const conn = currentConnection();
  const wallet = walletStore.get();
  if (!conn || !wallet) return null;
  try {
    return journalFor(conn, wallet);
  } catch {
    return null;
  }
}

function context(conn: Connection, wallet: ConnectedWallet, journal: Journal): Context {
  return {
    node: conn.node,
    params: conn.params,
    genesis: conn.genesis,
    network: conn.network,
    endpoint: conn.endpoint,
    wallet,
    journal,
    isCurrent: () => currentConnection() === conn && walletStore.get() === wallet,
    pendingOperations: () => activityStore.get()
      .filter((a) => a.entry.account === wallet.account.account && (a.status === null || a.status.status === 'pending'))
      .map((a) => ({ opId: a.opId, nonce: BigInt(a.entry.nonce) })),
    onSubmitted: (_op: VmOp, _id: string) => {},
  };
}

function setReview(next: Review | null) {
  reviewStore.set(next);
}

function patchReview(id: number, patch: Partial<Review>) {
  const r = reviewStore.get();
  if (r && r.id === id) setReview({ ...r, ...patch });
}

async function gasFor(conn: Connection, wallet: ConnectedWallet, request: OpRequest): Promise<{ gas: bigint; simulation?: Receipt }> {
  const a = request.action;
  if (request.gas !== undefined) return { gas: request.gas };
  if (a.kind === 'Deploy') return { gas: deployGas(a.code.length) };
  if (a.kind === 'AssetTransfer') return { gas: 10_000n };
  const simulation = await conn.node.call({
    contract: a.contract, method: a.method, args: a.args, funds: a.funds, caller: wallet.account.account,
  });
  return { gas: simulation.success ? callGas(simulation.gas_used, conn.params) : DEFAULT_GAS, simulation };
}

/** Opens the review of an operation; resolves once a block includes it */
export function runOp(request: OpRequest): Promise<Activity> {
  const conn = currentConnection();
  const wallet = walletStore.get();
  if (!conn) return Promise.reject(new PlaygroundError('network'));
  if (!wallet) return Promise.reject(new PlaygroundError('changed', 'wallet'));
  if (pendingReview) pendingReview.reject(new CancelledError());
  const id = ++reviewId;
  setReview({ id, request, phase: 'preparing' });
  const done = new Promise<Activity>((resolve, reject) => {
    pendingReview = { id, resolve, reject };
  });
  void (async () => {
    try {
      const journal = journalFor(conn, wallet);
      const c = context(conn, wallet, journal);
      const { gas, simulation } = await gasFor(conn, wallet, request);
      const prepared = await withWriteLock(journal, () => prepare(c, request.action, gas));
      patchReview(id, { phase: 'ready', prepared, context: c, ...(simulation ? { simulation } : {}) });
    } catch (error) {
      patchReview(id, { phase: 'failed', error });
    }
  })();
  return done;
}

export function cancelReview(): void {
  const p = pendingReview;
  pendingReview = null;
  setReview(null);
  p?.reject(new CancelledError());
}

/** Signs and sends the reviewed operation, then follows it until a block includes it */
export async function confirmReview(): Promise<void> {
  const r = reviewStore.get();
  const p = pendingReview;
  if (!r || !p || r.id !== p.id || r.phase !== 'ready' || !r.prepared || !r.context) return;
  patchReview(r.id, { phase: 'signing' });
  const c = r.context;
  let result: { entry: Entry; error?: unknown };
  try {
    result = await withWriteLock(c.journal, () => submit(c, r.prepared!));
  } catch (error) {
    patchReview(r.id, { phase: 'failed', error });
    return;
  }
  if (result.entry.state === 'rejected' || result.entry.state === 'unknown') {
    addActivity({ opId: result.entry.opId, title: r.request.title, entry: result.entry, status: null, error: result.error });
    patchReview(r.id, { phase: 'failed', error: result.error });
    return;
  }
  pendingReview = null;
  setReview(null);
  const activity: Activity = { opId: result.entry.opId, title: r.request.title, entry: result.entry, status: null };
  addActivity(activity);
  void refreshAccount();
  track(c, activity).then(p.resolve, p.reject);
}

function addActivity(a: Activity) {
  activityStore.update((list) => [a, ...list.filter((x) => x.opId !== a.opId)].slice(0, 100));
}

function patchActivity(opId: string, patch: Partial<Activity>): Activity | null {
  let next: Activity | null = null;
  activityStore.update((list) => list.map((a) => {
    if (a.opId !== opId) return a;
    next = { ...a, ...patch };
    return next;
  }));
  return next;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Polls the operation's status until a block includes it (or the node gives up on it) */
async function track(c: Context, activity: Activity): Promise<Activity> {
  let failures = 0;
  for (let attempt = 0; attempt < 240; attempt++) {
    await sleep(attempt === 0 ? 1_500 : 2_500);
    if (connectionStore.get().state !== 'ready') break;
    try {
      const status = await c.node.getOpStatus(activity.opId);
      failures = 0;
      const now = patchActivity(activity.opId, { status }) ?? { ...activity, status };
      if (status.status !== 'pending' && status.status !== 'skipped') {
        void withWriteLock(c.journal, async () => c.journal.observe(activity.opId, 'included')).catch(() => {});
        void refreshAccount();
        if (status.status === 'failed' || (status.receipt && !status.receipt.success)) throw new OpFailedError(now);
        return now;
      }
    } catch (e) {
      if (e instanceof OpFailedError) throw e;
      failures++;
      if (failures >= 5) {
        patchActivity(activity.opId, { error: e });
        throw e;
      }
    }
  }
  throw new PlaygroundError('pending', activity.opId);
}

/** Refreshes one activity's status (the Activity panel's button) */
export async function refreshActivity(opId: string): Promise<void> {
  const conn = currentConnection();
  if (!conn) return;
  try {
    const status = await conn.node.getOpStatus(opId);
    patchActivity(opId, { status, missing: false });
  } catch (e) {
    patchActivity(opId, { error: e, missing: true });
  }
}

/** Loads the journal's entries the session does not show yet (after a reload) */
export function loadJournal(): void {
  const journal = currentJournal();
  if (!journal) return;
  let rows: Entry[];
  try {
    rows = journal.read();
  } catch {
    return;
  }
  const known = new Set(activityStore.get().map((a) => a.opId));
  const added = rows.filter((r) => !known.has(r.opId)).map((entry): Activity => ({
    opId: entry.opId, title: entry.kind === 'Call' ? entry.method ?? 'call' : entry.kind === 'Deploy' ? 'deploy' : 'transfer', entry, status: null,
  }));
  if (added.length) activityStore.update((list) => [...list, ...added].sort((a, b) => b.entry.at - a.entry.at).slice(0, 100));
}

/** Releases a nonce held by an operation the node no longer knows (explicit, after the person confirms) */
export async function releaseActivity(opId: string): Promise<void> {
  const conn = currentConnection();
  const wallet = walletStore.get();
  if (!conn || !wallet) throw new PlaygroundError('changed');
  const journal = journalFor(conn, wallet);
  const entry = await releaseNonce(context(conn, wallet, journal), opId, true);
  patchActivity(opId, { entry });
}

walletStore.subscribe(() => {
  activityStore.set([]);
  loadJournal();
});
connectionStore.subscribe(() => {
  activityStore.set([]);
  loadJournal();
});

export function useReview(): Review | null {
  return useStore(reviewStore);
}

export function useActivity(): Activity[] {
  return useStore(activityStore);
}
