/** The collection's values as the ABI decodes them, the checks the page makes before a send, and the reads of its token lists. */
import { useCallback, useEffect, useRef, useState } from 'react';
import { toHex } from '@ksana/sdk';
import { serialOf, tokenIdOf } from '../../abi/text';
import type { Abi, Value } from '../../abi/types';
import { useStore } from '../../app/store';
import { parseUnits } from '../../chain/assets';
import { useConnection, useTip, type Connection } from '../../chain/connection';
import { changesStore, readMethod } from '../../chain/views';
import { useWallet } from '../../chain/wallet';

export interface Listing {
  asset: Uint8Array;
  price: bigint;
}

/**
 * A token as the page keeps it: `id` is its number `n`, the collection's `n`-th mint, whose KS-721 ID is `ks721::id(n)`
 * ({@link idArg}); the views return the 32-byte ID.
 */
export interface Token {
  id: bigint;
  owner: Uint8Array;
  uri: string | null;
  approved: Uint8Array | null;
  listing: Listing | null;
}

/** A token as the views return it */
interface TokenValue extends Omit<Token, 'id'> {
  id: Uint8Array;
}

/** The page's token: null for an ID that is no mint's of this collection, which never mints one */
function asToken(value: TokenValue | null): Token | null {
  const id = value ? serialOf(value.id) : null;
  return value && id !== null ? { ...value, id } : null;
}

/** The KS-721 ID of the `n`-th token, as the methods take it */
export function idArg(id: bigint): Uint8Array {
  return tokenIdOf(id);
}

export interface Collection {
  name: string;
  symbol: string;
  minter: Uint8Array | null;
  minted: bigint;
  supply: bigint;
  max_supply: bigint | null;
}

/** `MAX_URI_BYTES` of the contract */
export const MAX_URI_BYTES = 512;
/** Cards a list shows at first, and adds with each More */
export const PAGE = 12;
/** The most tokens one call of a list view returns (`MAX_PAGE` of the contract) */
const CALL_LIMIT = 50;

const utf8 = new TextEncoder();

export function utf8Bytes(text: string): number {
  return utf8.encode(text).length;
}

export function isZero(account: Uint8Array): boolean {
  return account.every((b) => b === 0);
}

/** A count as the page writes it: thousands grouped the same way in every language */
export function count(n: bigint): string {
  return n.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/** A token's number typed by a person (`12` or `#12`): 1 or more and within u64, else null */
export function typedId(text: string): bigint | null {
  const t = text.trim().replace(/^#/, '');
  if (!/^\d{1,20}$/.test(t)) return null;
  const id = BigInt(t);
  return id >= 1n && id < 1n << 64n ? id : null;
}

/** A price typed in the asset's units, in base units: null when empty, not an amount, or 0 */
export function priceOf(text: string, decimals: number): bigint | null {
  if (!text.trim()) return null;
  try {
    const v = parseUnits(text, decimals);
    return v > 0n ? v : null;
  } catch {
    return null;
  }
}

/** Whether the text is an amount that comes to 0 (the contract refuses a price of 0) */
export function isZeroAmount(text: string, decimals: number): boolean {
  if (!text.trim()) return false;
  try {
    return parseUnits(text, decimals) === 0n;
  } catch {
    return false;
  }
}

/** 64 hex digits (with or without 0x) as lower-case hex, else null */
export function assetHexOf(text: string): string | null {
  const t = text.trim().toLowerCase().replace(/^0x/, '');
  return /^[0-9a-f]{64}$/.test(t) ? t : null;
}

/** The address of an https URI without a user name or password in it, else null: any other URI is shown as text only */
export function httpsLink(uri: string): string | null {
  if (!/^\s*https:\/\//i.test(uri)) return null;
  try {
    const url = new URL(uri.trim());
    return url.protocol === 'https:' && !url.username && !url.password ? url.href : null;
  } catch {
    return null;
  }
}

/** A hue for a token's badge, spread around the colour wheel by its ID */
export function hueOf(id: bigint): number {
  return Number((id * 137n) % 360n);
}

/** Which list view to page through by position: an owner's tokens, the listings, or every token */
export type ListSource = { view: 'details_of'; owner: Uint8Array } | { view: 'listings' } | { view: 'details' };

interface ListRead {
  tokens: Token[];
  /** Tokens follow the ones read */
  more: boolean;
}

async function readList(conn: Connection, contract: string, abi: Abi, source: ListSource, want: number, caller: string | undefined): Promise<ListRead> {
  const opts = { caller };
  const call = async (start: bigint, limit: number): Promise<Token[]> => {
    const args: Value[] = source.view === 'details_of' ? [source.owner, start, BigInt(limit)] : [start, BigInt(limit)];
    const page = (await readMethod(conn, contract, abi, source.view, args, opts)) as unknown as TokenValue[];
    return page.map(asToken).filter((token) => token !== null);
  };
  const out: Token[] = [];
  // By position, one more than is shown: it tells whether more follow
  for (let start = 0n; out.length <= want;) {
    const limit = Math.min(CALL_LIMIT, want + 1 - out.length);
    const page = await call(start, limit);
    out.push(...page);
    start += BigInt(limit);
    if (page.length < limit) break;
  }
  // a block between two calls can move a token to another position: show it once
  const seen = new Set<bigint>();
  const unique = out.filter((token) => !seen.has(token.id) && !!seen.add(token.id));
  return { tokens: unique.slice(0, want), more: unique.length > want };
}

export interface TokenList {
  /** null until the first read for this list arrives, or after a failed one */
  tokens: Token[] | null;
  more: boolean;
  error: string | null;
  /** More was pressed and its read has not arrived yet */
  growing: boolean;
  showMore: () => void;
}

/**
 * A list view read call after call from its start, again on every block and after every operation of this page (as
 * `useView` reads), because removing a token from a list moves another into its position. More shows `PAGE` more.
 */
export function useTokenList(contract: string, abi: Abi, source: ListSource | null): TokenList {
  const conn = useConnection();
  const tip = useTip();
  const wallet = useWallet();
  const changes = useStore(changesStore);
  const key = source === null ? '' : source.view === 'details_of' ? `details_of:${toHex(source.owner)}` : source.view;
  const [pages, setPages] = useState({ key, n: 1 });
  const n = pages.key === key ? pages.n : 1;
  const [state, setState] = useState<{ key: string; want: number; read: ListRead | null; error: string | null } | null>(null);
  const sourceRef = useRef(source);
  sourceRef.current = source;
  const caller = wallet?.account.account;
  const height = tip?.tip.height;
  useEffect(() => {
    const src = sourceRef.current;
    if (!conn || !src) return;
    let alive = true;
    const want = n * PAGE;
    readList(conn, contract, abi, src, want, caller).then(
      (read) => { if (alive) setState({ key, want, read, error: null }); },
      (e: unknown) => { if (alive) setState({ key, want, read: null, error: e instanceof Error ? e.message : String(e) }); },
    );
    return () => { alive = false; };
  }, [conn, contract, abi, key, n, caller, height, changes]);
  const showMore = useCallback(() => setPages({ key, n: n + 1 }), [key, n]);
  const current = state && state.key === key && source ? state : null;
  return {
    tokens: current?.read?.tokens ?? null,
    more: current?.read?.more ?? false,
    error: current?.error ?? null,
    growing: !!current && current.want < n * PAGE,
    showMore,
  };
}

/**
 * One token by ID, read like `useView` reads, but with the answer kept with its ID: a token is never shown under another's
 * ID while the new one is read, and `undefined` (not read yet) is told apart from `null` (no such token).
 */
export function useToken(contract: string, abi: Abi, id: bigint | null): { token: Token | null | undefined; error: string | null } {
  const conn = useConnection();
  const tip = useTip();
  const wallet = useWallet();
  const changes = useStore(changesStore);
  const [state, setState] = useState<{ id: bigint; token: Token | null; error: string | null } | null>(null);
  const caller = wallet?.account.account;
  const height = tip?.tip.height;
  useEffect(() => {
    if (!conn || id === null) return;
    let alive = true;
    readMethod(conn, contract, abi, 'token', [idArg(id)], { caller }).then(
      (value) => { if (alive) setState({ id, token: asToken(value as unknown as TokenValue | null), error: null }); },
      (e: unknown) => { if (alive) setState({ id, token: null, error: e instanceof Error ? e.message : String(e) }); },
    );
    return () => { alive = false; };
  }, [conn, contract, abi, id, caller, height, changes]);
  if (!state || id === null || state.id !== id) return { token: undefined, error: null };
  return state.error ? { token: undefined, error: state.error } : { token: state.token, error: null };
}
