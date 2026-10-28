/**
 * What happened on a contract lately: the node lists the operations that called it (ksana_getAccountOps of the contract's ID,
 * D-111), each op's receipt carries its events, and the ABI decodes them. An op that reaches the contract through another one,
 * as a KS-20 token's `transfer_and_call` calls a pool's `on_token_received`, is listed under the contract it called: a page
 * names such contracts as `via`, and their listings are read too. A deployment is never listed under the contract it deploys,
 * so its `init`'s events, a token's first mint among them, come through the node's index of the standards' events (D-120), for
 * a contract that declared a standard there. Read again when a block arrives, at most every 10 s, and at once after an
 * operation of this page; a read under way is never dropped for a newer one, which follows it instead.
 */
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { toHex, type OpPosition, type OpRef } from '@ksana/sdk';
import { decodeEvent } from '../abi/codec';
import type { Abi, Value } from '../abi/types';
import { useConnection, useTip, type Connection } from '../chain/connection';
import { explorerLink } from '../chain/deployment';
import { useStore } from '../app/store';
import { changesStore } from '../chain/views';
import { useT } from '../i18n';
import { Empty, Section } from './ui';
import { ValueView } from './values';

export interface ContractEvent {
  name: string;
  fields: { name: string; value: Value }[];
  /** By field name */
  values: Record<string, Value>;
  opId: string;
  height: bigint;
}

/** Newest first: by height, then slot and entry on the main chain */
function newestFirst(x: OpRef, y: OpRef): number {
  const [p, q] = [x.position, y.position];
  if (p.height !== q.height) return p.height > q.height ? -1 : 1;
  return q.slot - p.slot || q.entry - p.entry;
}

function samePosition(p: OpPosition, q: OpPosition): boolean {
  return p.height === q.height && p.slot === q.slot && p.entry === q.entry;
}

/**
 * The op that deployed a contract which declares its standard in its `init` (docs/STANDARDS.md §2): the node's index gives the
 * position of the op whose events hold the declaration (`declared`), and the listing of the whole chain from just past it starts
 * with that op (docs/RPC.md). `null` for a contract without a declaration, and from a node without the index, which answers
 * `-32601`: the page then shows what the listings give, as before D-120.
 */
async function deployment(conn: Connection, contract: string): Promise<OpRef | null> {
  try {
    const { declared } = await conn.node.getContract(contract);
    if (declared === null) return null;
    const page = await conn.node.getLatestOps({ before: { ...declared, entry: declared.entry + 1 }, limit: 1 });
    const ref = page.ops[0];
    return ref !== undefined && samePosition(ref.position, declared) ? ref : null;
  } catch {
    return null;
  }
}

/** The contract's events in the latest `limit` operations that called it or one of `via`, and its deployment, newest first */
export async function readEvents(conn: Connection, contract: string, abi: Abi, limit: number, via: readonly string[]): Promise<ContractEvent[]> {
  const pages = await Promise.all([contract, ...via].map((account) => conn.node.getAccountOps(account, { limit })));
  const refs = new Map<string, OpRef>();
  for (const page of pages) for (const ref of page.ops) refs.set(toHex(ref.op_id), ref);
  // The deployment comes before every op that reached the contract: it is among the latest `limit` only when the listings hold
  // all their ops with room to spare
  if (refs.size < limit && pages.every((page) => page.next === null)) {
    const ref = await deployment(conn, contract);
    if (ref) refs.set(toHex(ref.op_id), ref);
  }
  const out: ContractEvent[] = [];
  for (const ref of [...refs.values()].sort(newestFirst).slice(0, limit)) {
    const status = await conn.node.getOpStatus(ref.op_id);
    for (const ev of status.receipt?.events ?? []) {
      if (toHex(ev.contract) !== contract) continue;
      const decoded = decodeEvent(abi, ev.data);
      if (!decoded) continue;
      out.push({
        ...decoded,
        values: Object.fromEntries(decoded.fields.map((f) => [f.name, f.value])),
        opId: toHex(ref.op_id),
        height: ref.block.height,
      });
    }
  }
  return out;
}

/** The least time between two reads a new block asks for */
const BLOCK_READ_INTERVAL = 10_000;

export function useContractEvents(contract: string | null, abi: Abi | null, limit = 10, via: readonly string[] = []): { events: ContractEvent[]; loading: boolean } {
  const conn = useConnection();
  const height = useTip()?.tip.height;
  const changes = useStore(changesStore);
  const [events, setEvents] = useState<ContractEvent[]>([]);
  const [loading, setLoading] = useState(false);
  // Asks the reader of the current contract for a read: soon (a block arrived) or now (an operation of this page)
  const request = useRef<(now: boolean) => void>(() => {});
  const viaKey = via.join(',');
  useEffect(() => {
    setEvents([]);
    if (!conn || !contract || !abi) {
      setLoading(false);
      return;
    }
    let alive = true;
    let running = false;
    let again: 'soon' | 'now' | null = null;
    let last = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const read = async () => {
      running = true;
      last = Date.now();
      setLoading(true);
      try {
        const out = await readEvents(conn, contract, abi, limit, viaKey ? viaKey.split(',') : []);
        if (alive) setEvents(out);
      } catch {
        /* kept as they were; the next block tries again */
      } finally {
        running = false;
        if (alive) {
          setLoading(false);
          const next = again;
          again = null;
          if (next) schedule(next === 'now');
        }
      }
    };
    const schedule = (now: boolean) => {
      if (!alive) return;
      if (running) {
        if (again !== 'now') again = now ? 'now' : 'soon';
        return;
      }
      const wait = now ? 0 : Math.max(0, BLOCK_READ_INTERVAL - (Date.now() - last));
      if (timer !== undefined) {
        if (!now) return;
        clearTimeout(timer);
      }
      timer = setTimeout(() => {
        timer = undefined;
        void read();
      }, wait);
    };
    request.current = schedule;
    schedule(true);
    return () => {
      alive = false;
      if (timer !== undefined) clearTimeout(timer);
      request.current = () => {};
    };
  }, [conn, contract, abi, limit, viaKey]);
  const seen = useRef({ height, changes });
  useEffect(() => {
    const before = seen.current;
    seen.current = { height, changes };
    if (before.changes !== changes) request.current(true);
    else if (before.height !== height) request.current(false);
  }, [height, changes]);
  return { events, loading };
}

/** The contract's recent events, decoded; `only` keeps some of them, `render` draws one its own way, `via` as in useContractEvents */
export function ContractEvents({ contract, abi, title, only, render, limit = 10, via }: {
  contract: string; abi: Abi; title?: ReactNode; only?: string[]; render?: (e: ContractEvent) => ReactNode | null; limit?: number;
  via?: readonly string[];
}) {
  const t = useT();
  const { events } = useContractEvents(contract, abi, limit, via);
  const shown = only ? events.filter((e) => only.includes(e.name)) : events;
  return <Section title={title ?? t.core.methods.events}>
    {shown.length === 0 ? <Empty>{t.core.methods.noEvents}</Empty> : <ul className="events">
      {shown.map((e, i) => {
        const custom = render?.(e);
        const link = explorerLink('op', e.opId);
        return <li key={`${e.opId}-${i}`} className="event">
          <div className="kv">
            <span className="event-name mono">{e.name}</span>
            <span className="muted mono">{link ? <a href={link} target="_blank" rel="noreferrer">#{e.height.toString()}</a> : `#${e.height}`}</span>
          </div>
          {custom ?? <dl className="vrows">{e.fields.map((f, j) => {
            const type = abi.events.find((x) => x.name === e.name)?.fields[j]?.type;
            return <div key={f.name}><dt>{f.name}</dt><dd>{type ? <ValueView abi={abi} type={type} value={f.value} /> : '?'}</dd></div>;
          })}</dl>}
        </li>;
      })}
    </ul>}
  </Section>;
}
