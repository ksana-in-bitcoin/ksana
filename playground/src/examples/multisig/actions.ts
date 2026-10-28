/** The multisig wallet as the page reads it (decoded views), its actions in the contract's encoding, and the rules the contract checks. */
import { fromHex, toHex } from '@ksana/sdk';
import { isEnumValue, type Value } from '../../abi/types';

/** The contract's limits (contracts/examples/multisig/src/lib.rs) */
export const MAX_OWNERS = 20;
export const MAX_METHOD_BYTES = 64;
export const MAX_ARGS_BYTES = 16_384;
/** The most funds a `Call` attaches (the host's rule, which the contract checks when proposed) */
export const MAX_FUNDS = 16;
/** The most proposals one page of `proposals` returns */
export const MAX_PAGE_ITEMS = 50;

export const ZERO_ID = '00'.repeat(32);

export interface FundsItem {
  /** Asset ID (hex) */
  asset: string;
  amount: bigint;
}

/** What a proposal does once executed; accounts, contracts and assets as hex */
export type WalletAction =
  | { kind: 'Transfer'; asset: string; to: string; amount: bigint }
  | { kind: 'Call'; contract: string; method: string; args: Uint8Array; funds: FundsItem[] }
  | { kind: 'AddOwner'; account: string }
  | { kind: 'RemoveOwner'; account: string }
  | { kind: 'SetThreshold'; threshold: number };

export type ActionKind = WalletAction['kind'];
export const ACTION_KINDS: readonly ActionKind[] = ['Transfer', 'Call', 'AddOwner', 'RemoveOwner', 'SetThreshold'];

export type Status = 'Pending' | 'Executed' | 'Cancelled';

export interface Proposal {
  id: bigint;
  proposer: string;
  action: WalletAction;
  /** The approvals that count: of a pending proposal those of current owners in their current term, else those that counted then */
  approvals: string[];
  status: Status;
  /** The block timestamp when it was proposed (Unix seconds) */
  createdAt: bigint;
}

export interface WalletInfo {
  /** In the order they became owners */
  owners: string[];
  threshold: number;
  /** The latest proposal ID: how many proposals have been made */
  latest: bigint;
}

type Fields = { [field: string]: Value };

function fieldsOf(v: Value | undefined): Fields | null {
  return typeof v === 'object' && v !== null && !Array.isArray(v) && !(v instanceof Uint8Array) && !isEnumValue(v) ? (v as Fields) : null;
}

function idOf(v: Value | undefined): string | null {
  return v instanceof Uint8Array && v.length === 32 ? toHex(v) : null;
}

function idsOf(v: Value | undefined): string[] | null {
  if (!Array.isArray(v)) return null;
  const out: string[] = [];
  for (const item of v) {
    const id = idOf(item);
    if (id === null) return null;
    out.push(id);
  }
  return out;
}

function u8Of(v: Value | undefined): number | null {
  return typeof v === 'bigint' && v >= 0n && v <= 255n ? Number(v) : null;
}

function fundsItemOf(v: Value | undefined): FundsItem | null {
  const f = fieldsOf(v);
  const asset = idOf(f?.asset);
  return f && asset !== null && typeof f.amount === 'bigint' ? { asset, amount: f.amount } : null;
}

/** The action as the ABI codec takes it: `{ variant, values }` with the variant's fields in order */
export function actionValue(a: WalletAction): Value {
  switch (a.kind) {
    case 'Transfer':
      return { variant: 'Transfer', values: [fromHex(a.asset, 32), fromHex(a.to, 32), a.amount] };
    case 'Call':
      return {
        variant: 'Call',
        values: [fromHex(a.contract, 32), a.method, a.args, a.funds.map((f): Value => ({ asset: fromHex(f.asset, 32), amount: f.amount }))],
      };
    case 'AddOwner':
      return { variant: 'AddOwner', values: [fromHex(a.account, 32)] };
    case 'RemoveOwner':
      return { variant: 'RemoveOwner', values: [fromHex(a.account, 32)] };
    case 'SetThreshold':
      return { variant: 'SetThreshold', values: [BigInt(a.threshold)] };
  }
}

/** A decoded `Action`; null when it has another shape (a contract deployed from edited code) */
export function actionOf(v: Value | undefined): WalletAction | null {
  if (v === undefined || !isEnumValue(v)) return null;
  const [a, b, c, d] = v.values;
  switch (v.variant) {
    case 'Transfer': {
      const asset = idOf(a);
      const to = idOf(b);
      return asset !== null && to !== null && typeof c === 'bigint' && v.values.length === 3 ? { kind: 'Transfer', asset, to, amount: c } : null;
    }
    case 'Call': {
      const contract = idOf(a);
      if (contract === null || typeof b !== 'string' || !(c instanceof Uint8Array) || !Array.isArray(d) || v.values.length !== 4) return null;
      const funds: FundsItem[] = [];
      for (const item of d) {
        const f = fundsItemOf(item);
        if (!f) return null;
        funds.push(f);
      }
      return { kind: 'Call', contract, method: b, args: c, funds };
    }
    case 'AddOwner':
    case 'RemoveOwner': {
      const account = idOf(a);
      if (account === null || v.values.length !== 1) return null;
      return v.variant === 'AddOwner' ? { kind: 'AddOwner', account } : { kind: 'RemoveOwner', account };
    }
    case 'SetThreshold': {
      const threshold = u8Of(a);
      return threshold !== null && v.values.length === 1 ? { kind: 'SetThreshold', threshold } : null;
    }
    default:
      return null;
  }
}

function statusOf(v: Value | undefined): Status | null {
  if (v === undefined || !isEnumValue(v) || v.values.length !== 0) return null;
  return v.variant === 'Pending' || v.variant === 'Executed' || v.variant === 'Cancelled' ? v.variant : null;
}

/** A decoded `Proposal`, or null */
export function proposalOf(v: Value | undefined): Proposal | null {
  const f = fieldsOf(v);
  if (!f) return null;
  const proposer = idOf(f.proposer);
  const action = actionOf(f.action);
  const approvals = idsOf(f.approvals);
  const status = statusOf(f.status);
  if (typeof f.id !== 'bigint' || proposer === null || !action || !approvals || !status || typeof f.created_at !== 'bigint') return null;
  return { id: f.id, proposer, action, approvals, status, createdAt: f.created_at };
}

/** One of the contract's events */
export type WalletEvent =
  | { kind: 'deposit'; from: string; asset: string; amount: bigint }
  | { kind: 'propose' | 'approve' | 'revoke' | 'execute'; id: bigint; who: string }
  | { kind: 'cancel'; id: bigint }
  | { kind: 'owner_added' | 'owner_removed'; owner: string }
  | { kind: 'threshold'; threshold: number };

/** An event from its decoded fields by name; null for another event or shape */
export function eventOf(name: string, values: Readonly<Record<string, Value>>): WalletEvent | null {
  const id = values.id;
  switch (name) {
    case 'deposit': {
      const from = idOf(values.from);
      const asset = idOf(values.asset);
      const amount = values.amount;
      return from !== null && asset !== null && typeof amount === 'bigint' ? { kind: 'deposit', from, asset, amount } : null;
    }
    case 'propose':
    case 'approve':
    case 'revoke':
    case 'execute': {
      const who = idOf(name === 'propose' ? values.proposer : name === 'execute' ? values.executor : values.owner);
      if (who === null || typeof id !== 'bigint') return null;
      const kind = name === 'propose' ? 'propose' : name === 'approve' ? 'approve' : name === 'revoke' ? 'revoke' : 'execute';
      return { kind, id, who };
    }
    case 'cancel':
      return typeof id === 'bigint' ? { kind: 'cancel', id } : null;
    case 'owner_added':
    case 'owner_removed': {
      const owner = idOf(values.owner);
      if (owner === null) return null;
      return name === 'owner_added' ? { kind: 'owner_added', owner } : { kind: 'owner_removed', owner };
    }
    case 'threshold': {
      const threshold = u8Of(values.threshold);
      return threshold === null ? null : { kind: 'threshold', threshold };
    }
    default:
      return null;
  }
}

/** The decoded `Wallet` view, or null */
export function walletOf(v: Value | undefined): WalletInfo | null {
  const f = fieldsOf(v);
  if (!f) return null;
  const owners = idsOf(f.owners);
  const threshold = u8Of(f.threshold);
  if (!owners || threshold === null || typeof f.proposals !== 'bigint') return null;
  return { owners, threshold, latest: f.proposals };
}

const utf8 = new TextEncoder();

export function utf8Length(text: string): number {
  return utf8.encode(text).length;
}

/** A method name a `Call` can name: 1 to 64 bytes, not `init` */
export function methodOk(method: string): boolean {
  const n = utf8Length(method);
  return n >= 1 && n <= MAX_METHOD_BYTES && method !== 'init';
}

/** Why the contract would refuse an action, in the words of its revert reasons */
export type Problem =
  | 'zeroRecipient' | 'zeroAmount' | 'selfCall' | 'zeroContract' | 'method' | 'argsTooLong' | 'tooManyFunds' | 'zeroFunds' | 'fundsOrder'
  | 'zeroOwner' | 'selfOwner' | 'alreadyOwner' | 'tooManyOwners' | 'unknownOwner' | 'belowThreshold' | 'threshold';

/** The host's rules for attached funds as the contract checks them: at most 16, amounts above 0, asset IDs strictly ascending */
export function fundsProblem(funds: readonly FundsItem[]): Problem | null {
  if (funds.length > MAX_FUNDS) return 'tooManyFunds';
  if (funds.some((f) => f.amount === 0n)) return 'zeroFunds';
  for (let i = 1; i < funds.length; i++) {
    if (!(funds[i - 1]!.asset < funds[i]!.asset)) return 'fundsOrder';
  }
  return null;
}

/** Funds rows in the order the contract takes them (lowercase hex sorts as the bytes do), or why they cannot be */
export function sortedFunds(items: readonly FundsItem[]): { funds: FundsItem[]; problem: Problem | null } {
  const funds = [...items].sort((a, b) => (a.asset < b.asset ? -1 : a.asset > b.asset ? 1 : 0));
  return { funds, problem: fundsProblem(funds) };
}

/**
 * The contract's `check_action`, in its order: an action's shape, and an owner action against the owners and the threshold of
 * the moment. It runs when an action is proposed and again when it is executed. `self` is the wallet's contract ID.
 */
export function checkAction(a: WalletAction, w: WalletInfo, self: string): Problem | null {
  switch (a.kind) {
    case 'Transfer':
      if (a.to === ZERO_ID) return 'zeroRecipient';
      if (a.amount === 0n) return 'zeroAmount';
      return null;
    case 'Call':
      if (a.contract === self) return 'selfCall';
      if (a.contract === ZERO_ID) return 'zeroContract';
      if (!methodOk(a.method)) return 'method';
      if (a.args.length > MAX_ARGS_BYTES) return 'argsTooLong';
      return fundsProblem(a.funds);
    case 'AddOwner':
      if (a.account === ZERO_ID) return 'zeroOwner';
      if (a.account === self) return 'selfOwner';
      if (w.owners.includes(a.account)) return 'alreadyOwner';
      if (w.owners.length >= MAX_OWNERS) return 'tooManyOwners';
      return null;
    case 'RemoveOwner':
      if (!w.owners.includes(a.account)) return 'unknownOwner';
      if (w.owners.length - 1 < w.threshold) return 'belowThreshold';
      return null;
    case 'SetThreshold':
      if (a.threshold < 1 || a.threshold > w.owners.length) return 'threshold';
      return null;
  }
}

/** What an action takes from the wallet's balances when it runs */
export function fundsNeeded(a: WalletAction): FundsItem[] {
  if (a.kind === 'Transfer') return [{ asset: a.asset, amount: a.amount }];
  if (a.kind === 'Call') return a.funds;
  return [];
}

export type Shortfall =
  | { kind: 'balance'; asset: string; held: bigint; needed: bigint }
  /** A balance it needs is not read yet */
  | { kind: 'reading' };

export type Blocker = { kind: 'approvals'; missing: number } | { kind: 'rule'; problem: Problem } | Shortfall;

/** The first of the wallet's balances that does not cover what the action takes, as `execute` checks them */
export function shortfall(a: WalletAction, balances: ReadonlyMap<string, bigint> | null): Shortfall | null {
  for (const f of fundsNeeded(a)) {
    const held = balances?.get(f.asset);
    if (held === undefined) return { kind: 'reading' };
    if (held < f.amount) return { kind: 'balance', asset: f.asset, held, needed: f.amount };
  }
  return null;
}

/**
 * Why `execute` would revert now, in the contract's order of checks (the approvals of current owners against the threshold, the
 * action against the state of the moment, the wallet's balance of what it takes); null when it would go ahead. A pending
 * proposal's `approvals` are already only those that count.
 */
export function executeBlocker(p: Proposal, w: WalletInfo, self: string, balances: ReadonlyMap<string, bigint> | null): Blocker | null {
  if (p.approvals.length < w.threshold) return { kind: 'approvals', missing: w.threshold - p.approvals.length };
  const problem = checkAction(p.action, w, self);
  if (problem) return { kind: 'rule', problem };
  return shortfall(p.action, balances);
}

/** Bytes typed as hex (spaces and a `0x` prefix allowed), or null when they are not */
export function parseHexBytes(text: string): Uint8Array | null {
  let t = text.replace(/\s+/g, '');
  if (t.startsWith('0x') || t.startsWith('0X')) t = t.slice(2);
  if (!/^(?:[0-9a-fA-F]{2})*$/.test(t)) return null;
  return fromHex(t);
}

/** A 32-byte ID typed as 64 hex digits (a `0x` prefix allowed), as lowercase hex; null otherwise */
export function idHexOf(text: string): string | null {
  let t = text.trim();
  if (t.startsWith('0x') || t.startsWith('0X')) t = t.slice(2);
  return /^[0-9a-fA-F]{64}$/.test(t) ? t.toLowerCase() : null;
}

/** An asset typed as KSA or its ID, as hex; null otherwise */
export function assetHexOf(text: string): string | null {
  return /^\s*ksa\s*$/i.test(text) ? ZERO_ID : idHexOf(text);
}
