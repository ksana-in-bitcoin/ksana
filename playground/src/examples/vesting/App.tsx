/** Vesting: the schedules that pay you and what you can release, creating one, the ones you created, any schedule by ID, and recent events. */
import { useEffect, useId, useMemo, useState } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import type { Abi, Value } from '../../abi/types';
import { useConnection } from '../../chain/connection';
import { useView, type ViewState } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useLocale, useT } from '../../i18n';
import { ContractEvents, type ContractEvent } from '../../ui/Events';
import { Banner, Button, Empty, Field, Rows, Section, Stat } from '../../ui/ui';
import { AccountText, Amount } from '../../ui/values';
import { HowItWorks } from '../Prose';
import type { AppProps } from '../kit';
import { ScheduleCard, Sentence } from './Card';
import { CreatePanel } from './Create';
import { rememberAssets, stateKey, useChainTime, useReleasable, useScheduleAsset, type ChainTime } from './reads';
import { asSchedule, asSchedules, dateLocale, formatDateTime, parseId, releasableAt, vestedAt } from './schedule';
import './vesting.css';

/** Schedules per page of a list (the contract gives at most 50 at a time) */
const PAGE = 10;
const EVENTS = ['create', 'release', 'revoke'];

export default function App({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.vesting;
  const wallet = useWallet();
  const me = useMemo(() => (wallet ? fromHex(wallet.account.account, 32) : null), [wallet]);
  const time = useChainTime();
  const count = useView<bigint>(contract, abi, 'count');
  const [lookup, setLookup] = useState('');
  const meHex = me ? toHex(me) : '';
  return <div className="app-grid">
    <div className="app-col">
      {me
        ? <ScheduleList key={`beneficiary:${meHex}`} contract={contract} abi={abi} me={me} now={time.now} kind="beneficiary" />
        : <Section title={m.mine}><p className="field-hint">{m.connectToSee}</p></Section>}
      <CreatePanel contract={contract} abi={abi} now={time.now} onCreated={(id) => setLookup(id.toString())} />
      {me && <ScheduleList key={`creator:${meHex}`} contract={contract} abi={abi} me={me} now={time.now} kind="creator" />}
    </div>
    <div className="app-col">
      <Overview count={count} time={time} />
      <Lookup contract={contract} abi={abi} me={me} now={time.now} count={count.data} value={lookup} onChange={setLookup} />
      <ContractEvents contract={contract} abi={abi} title={m.recent}
        render={(e) => (EVENTS.includes(e.name) ? <EventLine e={e} contract={contract} abi={abi} /> : null)} />
      <HowItWorks title={m.howTitle} items={m.how} />
    </div>
  </div>;
}

function Overview({ count, time }: { count: ViewState<bigint>; time: ChainTime }) {
  const m = useT().vesting;
  const locale = dateLocale(useLocale());
  return <Section title={m.overview}>
    <div className="vesting-stack">
      <div className="stats">
        <Stat label={m.count} value={count.data !== null ? count.data.toString() : '…'} sub={m.countSub} />
      </div>
      <Rows rows={[{
        label: m.chainTime,
        value: <span className="vesting-time">
          <span>{formatDateTime(time.now, locale)}</span>
          <span className="muted">{time.chain && time.height !== null ? m.atBlock(time.height.toString()) : m.yourClock}</span>
        </span>,
      }]} />
      <p className="field-hint">{m.clockHint}</p>
      {count.error && <Banner tone="danger">{count.error}</Banner>}
    </div>
  </Section>;
}

/** Amounts of several assets, one per line */
function Amounts({ values }: { values: [string, bigint][] }) {
  return <span className="vesting-amounts">{values.map(([asset, v]) => <Amount key={asset} value={v} asset={asset} />)}</span>;
}

/** The schedules that pay you, or the ones you created, a page at a time in the order they were created */
function ScheduleList({ contract, abi, me, now, kind }: { contract: string; abi: Abi; me: Uint8Array; now: bigint; kind: 'beneficiary' | 'creator' }) {
  const t = useT();
  const m = t.vesting;
  const conn = useConnection();
  const [start, setStart] = useState(0n);
  // One more than a page, to know whether another follows
  const list = useView<Value>(contract, abi, kind === 'beneficiary' ? 'by_beneficiary' : 'by_creator', [me, start, BigInt(PAGE + 1)]);
  const page = useMemo(() => asSchedules(list.data), [list.data]);
  const shown = useMemo(() => page?.slice(0, PAGE) ?? null, [page]);
  const more = !!page && page.length > PAGE;
  const releasable = useReleasable(contract, abi, shown);
  useEffect(() => {
    if (shown) rememberAssets(conn, contract, shown);
  }, [conn, contract, shown]);

  // What waits on you across the page, by asset
  const due = new Map<string, bigint>();
  const locked = new Map<string, bigint>();
  for (const s of kind === 'beneficiary' ? shown ?? [] : []) {
    const known = releasable.get(stateKey(s));
    const vested = known !== undefined ? s.released + known : vestedAt(s, now);
    const asset = toHex(s.asset);
    due.set(asset, (due.get(asset) ?? 0n) + (known ?? releasableAt(s, now)));
    locked.set(asset, (locked.get(asset) ?? 0n) + (s.total > vested ? s.total - vested : 0n));
  }
  const partial = more || start > 0n ? m.onThisPage : undefined;

  return <Section title={kind === 'beneficiary' ? m.mine : m.createdList}>
    <div className="vesting-stack">
      <p className="field-hint">{kind === 'beneficiary' ? m.mineHint : m.createdHint}</p>
      {list.error && <Banner tone="danger">{list.error}</Banner>}
      {!shown ? (!list.error && <p className="muted">{t.core.common.loading}</p>)
        : shown.length === 0 ? <Empty>{kind === 'beneficiary' ? m.noneMine : m.noneCreated}</Empty>
        : <>
          {kind === 'beneficiary' && <div className="stats">
            <Stat label={m.dueTotal} value={<Amounts values={[...due]} />} sub={partial} />
            <Stat label={m.lockedTotal} value={<Amounts values={[...locked]} />} sub={partial} />
          </div>}
          <div className="list">
            {shown.map((s) => <ScheduleCard key={s.id.toString()} contract={contract} abi={abi} schedule={s} now={now}
              releasable={releasable.get(stateKey(s))} me={me} />)}
          </div>
        </>}
      {shown && (start > 0n || more) && <div className="vesting-pager">
        <Button size="sm" disabled={start === 0n} onClick={() => setStart(start > BigInt(PAGE) ? start - BigInt(PAGE) : 0n)}>{m.previous}</Button>
        <span className="muted mono">{m.range((start + 1n).toString(), (start + BigInt(shown.length)).toString())}</span>
        <Button size="sm" disabled={!more} onClick={() => setStart(start + BigInt(PAGE))}>{m.next}</Button>
      </div>}
    </div>
  </Section>;
}

/** Any schedule by its ID */
function Lookup({ contract, abi, me, now, count, value, onChange }: {
  contract: string; abi: Abi; me: Uint8Array | null; now: bigint; count: bigint | null; value: string; onChange: (v: string) => void;
}) {
  const t = useT();
  const m = t.vesting;
  const conn = useConnection();
  const fieldId = useId();
  const id = parseId(value);
  // IDs run from 1 to the count and schedules are never removed: past it there is nothing to ask for
  const beyond = id !== null && count !== null && id > count;
  const view = useView<Value>(contract, abi, 'schedule', id !== null ? [id] : [], { enabled: id !== null && !beyond });
  const found = useMemo(() => asSchedule(view.data ?? undefined), [view.data]);
  // The answer for the ID asked, not the one before it
  const schedule = found && found.id === id ? found : null;
  const one = useMemo(() => (schedule ? [schedule] : null), [schedule]);
  const releasable = useReleasable(contract, abi, one);
  useEffect(() => {
    if (one) rememberAssets(conn, contract, one);
  }, [conn, contract, one]);
  const hint = count === null ? undefined : count > 0n ? m.lookupHint(count.toString()) : m.lookupEmpty;
  return <Section title={m.lookup}>
    <div className="form-grid">
      <Field label={m.lookupId} htmlFor={fieldId} hint={hint} error={value.trim() && id === null ? m.badId : null}>
        <input id={fieldId} className="input mono" inputMode="numeric" autoComplete="off" placeholder="1" value={value}
          onChange={(e) => onChange(e.target.value)} />
      </Field>
      {id !== null && (beyond ? <Empty>{m.notFound}</Empty>
        : view.error ? <Banner tone="danger">{view.error}</Banner>
        : schedule ? <ScheduleCard contract={contract} abi={abi} schedule={schedule} now={now} releasable={releasable.get(stateKey(schedule))} me={me} chart />
        : <p className="muted">{t.core.common.loading}</p>)}
    </div>
  </Section>;
}

/** An event of the contract as a sentence */
function EventLine({ e, contract, abi }: { e: ContractEvent; contract: string; abi: Abi }) {
  const m = useT().vesting;
  const conn = useConnection();
  const id = typeof e.values.id === 'bigint' ? e.values.id : null;
  // A create names its asset; a release or a revoke only the schedule
  const named = e.name === 'create' && e.values.asset instanceof Uint8Array ? toHex(e.values.asset) : null;
  const looked = useScheduleAsset(contract, abi, named === null ? id : null);
  const asset = named ?? looked;
  const account = (v: Value | undefined) => (v instanceof Uint8Array && v.length === 32 ? <AccountText value={v} network={conn?.network} chars={6} /> : '?');
  const amount = (v: Value | undefined) => (typeof v !== 'bigint' ? '?' : asset ? <Amount value={v} asset={asset} /> : '…');
  const n = id?.toString() ?? '?';
  const parts = e.name === 'create' ? m.eventCreate(account(e.values.creator), amount(e.values.amount), n, account(e.values.beneficiary))
    : e.name === 'release' ? m.eventRelease(n, amount(e.values.amount), account(e.values.beneficiary))
    : e.values.refunded === 0n ? [m.eventRevokeNothing(n)]
    : m.eventRevoke(n, amount(e.values.refunded));
  return <Sentence className="vesting-sentence" parts={parts} />;
}
