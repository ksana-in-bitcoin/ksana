/** The page's reads that take several calls (every proposal, the assets the wallet holds and its balance of each) and asset names, re-read as `useView` re-reads a view. */
import { useEffect, useRef, useState } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import type { Abi } from '../../abi/types';
import { useStore } from '../../app/store';
import { KSA_HEX, KSA_META, assetMeta, type AssetMeta } from '../../chain/assets';
import { useConnection, useTip, type Connection } from '../../chain/connection';
import { changesStore, readMethod } from '../../chain/views';
import { t as messages } from '../../i18n';
import { errorText } from '../../ui/errors';
import { MAX_PAGE_ITEMS, proposalOf, type Proposal } from './actions';

/** A view returned something this page cannot read (a contract deployed from edited code, say) */
export class ShapeError extends Error {
  constructor(what: string) {
    super(what);
    this.name = 'ShapeError';
  }
}

export interface ReadState<T> {
  data: T | null;
  error: string | null;
}

/**
 * A read of several calls, run again on every block and after every send of this page; `key` names its inputs (null: nothing to
 * read). The last result stays while the next one loads, and when it fails.
 */
export function useRead<T>(key: string | null, read: (conn: Connection) => Promise<T>): ReadState<T> {
  const conn = useConnection();
  const tip = useTip();
  const changes = useStore(changesStore);
  const [state, setState] = useState<ReadState<T>>({ data: null, error: null });
  const readRef = useRef(read);
  readRef.current = read;
  const height = tip?.tip.height;
  useEffect(() => {
    if (!conn || key === null) {
      setState({ data: null, error: null });
      return;
    }
    let alive = true;
    readRef.current(conn).then(
      (data) => { if (alive) setState({ data, error: null }); },
      (e: unknown) => {
        if (!alive) return;
        const m = messages();
        setState((s) => ({ data: s.data, error: e instanceof ShapeError ? m.multisig.unexpected : errorText(e, m) }));
      },
    );
    return () => { alive = false; };
  }, [conn, key, height, changes]);
  return state;
}

/** At most this many calls per read of the proposals (1 000 proposals when they are small) */
const MAX_CALLS = 20;

export interface ProposalList {
  /** Newest first */
  list: Proposal[];
  /** False when the read stopped before the first proposal */
  complete: boolean;
}

/**
 * The proposals newest first: pages of `proposals(start, limit)` back from the latest ID, 50 IDs at a time. A page stops early
 * when the next proposal would take it past the contract's byte limit; the read goes on from there, so nothing is skipped.
 */
export async function readProposals(conn: Connection, contract: string, abi: Abi, latest: bigint): Promise<ProposalList> {
  const span = BigInt(MAX_PAGE_ITEMS);
  const list: Proposal[] = [];
  let calls = 0;
  let hi = latest;
  while (hi >= 1n) {
    const lo = hi > span ? hi - span + 1n : 1n;
    const part: Proposal[] = [];
    let next = lo;
    while (next <= hi) {
      if (calls >= MAX_CALLS) return { list, complete: false };
      calls++;
      const page = await readMethod(conn, contract, abi, 'proposals', [next, hi - next + 1n]);
      if (!Array.isArray(page)) throw new ShapeError('proposals');
      if (page.length === 0) break;
      for (const value of page) {
        const p = proposalOf(value);
        if (!p || p.id < next || p.id > hi) throw new ShapeError('proposal');
        part.push(p);
        next = p.id + 1n;
      }
    }
    list.push(...part.reverse());
    hi = lo - 1n;
  }
  return { list, complete: true };
}

export function useProposals(contract: string, abi: Abi, latest: bigint | null): ReadState<ProposalList> {
  return useRead<ProposalList>(latest === null ? null : latest.toString(),
    (conn) => (latest ? readProposals(conn, contract, abi, latest) : Promise.resolve({ list: [], complete: true })));
}

/**
 * The assets the wallet holds, as the node lists the balances of the contract's ID (`ksana_getAccount`): those deposited and
 * those sent to it with a plain transfer, which no event names
 */
export function useHoldings(contract: string): ReadState<string[]> {
  return useRead<string[]>(contract, async (conn) => (await conn.node.getAccount(contract)).balances.map((b) => toHex(b.asset)));
}

/** The wallet's `balance` of each asset, one call after another (a node runs only a few read-only calls at a time) */
export function useBalances(contract: string, abi: Abi, assets: readonly string[]): ReadState<ReadonlyMap<string, bigint>> {
  const key = assets.join(',');
  return useRead<ReadonlyMap<string, bigint>>(key, async (conn) => {
    const out = new Map<string, bigint>();
    for (const asset of key.split(',').filter(Boolean)) {
      const value = await readMethod(conn, contract, abi, 'balance', [fromHex(asset, 32)]);
      if (typeof value !== 'bigint') throw new ShapeError('balance');
      out.set(asset, value);
    }
    return out;
  });
}

/** Names and decimals of assets (KSA's known at once); one the node cannot tell yet is missing, and asked again with the next block */
export function useMetas(assets: readonly string[]): ReadonlyMap<string, AssetMeta> {
  const conn = useConnection();
  const tip = useTip();
  const key = assets.join(',');
  const height = tip?.tip.height;
  const [metas, setMetas] = useState<ReadonlyMap<string, AssetMeta>>(() => new Map([[KSA_HEX, KSA_META]]));
  useEffect(() => {
    if (!conn) return;
    let alive = true;
    const ids = key.split(',').filter(Boolean);
    void Promise.all(ids.map((id) => assetMeta(conn, id).catch(() => null))).then((found) => {
      if (!alive) return;
      const next = new Map<string, AssetMeta>([[KSA_HEX, KSA_META]]);
      for (const meta of found) if (meta) next.set(meta.id, meta);
      setMetas((prev) => (sameMetas(prev, next) ? prev : next));
    });
    return () => { alive = false; };
  }, [conn, key, height]);
  return metas;
}

function sameMetas(a: ReadonlyMap<string, AssetMeta>, b: ReadonlyMap<string, AssetMeta>): boolean {
  if (a.size !== b.size) return false;
  for (const [id, meta] of b) {
    const old = a.get(id);
    if (!old || old.name !== meta.name || old.decimals !== meta.decimals) return false;
  }
  return true;
}
