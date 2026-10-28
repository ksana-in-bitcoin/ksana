/** The wallet's proposals, newest first: each action in words, its approvals against the threshold, and what the connected owner can do now. */
import { useState } from 'react';
import { toHex } from '@ksana/sdk';
import type { Abi } from '../../abi/types';
import type { AssetMeta } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { useKnownContracts } from '../../chain/contracts';
import type { Detail } from '../../chain/run';
import { useLocale, useT } from '../../i18n';
import { Banner, Button, Empty, Meter, Section } from '../../ui/ui';
import { Amount } from '../../ui/values';
import { callAction } from '../kit';
import { checkAction, executeBlocker, shortfall, type Blocker, type Proposal, type WalletInfo } from './actions';
import { ActionWords, Who, Words, actionText, useAct } from './parts';
import type { ProposalList, ReadState } from './reads';

const PAGE = 10;

type Filter = 'pending' | 'all';

interface Props {
  contract: string;
  abi: Abi;
  info: WalletInfo;
  /** The connected account (hex) */
  me: string | null;
  balances: ReadonlyMap<string, bigint> | null;
  metas: ReadonlyMap<string, AssetMeta>;
}

export function ProposalsPanel({ proposals, ...props }: Props & { proposals: ReadState<ProposalList> }) {
  const t = useT();
  const m = t.multisig;
  const [filter, setFilter] = useState<Filter>('pending');
  const [page, setPage] = useState(0);
  const list = proposals.data?.list ?? null;
  const pending = list ? list.filter((p) => p.status === 'Pending') : [];
  const shown = filter === 'pending' ? pending : list ?? [];
  const pages = Math.max(1, Math.ceil(shown.length / PAGE));
  const at = Math.min(page, pages - 1);
  const items = shown.slice(at * PAGE, (at + 1) * PAGE);
  const choose = (f: Filter) => {
    setFilter(f);
    setPage(0);
  };
  const tab = (f: Filter, label: string) => <button type="button" className={filter === f ? 'is-active' : ''} aria-pressed={filter === f} onClick={() => choose(f)}>{label}</button>;
  return <Section title={m.proposals} actions={<div className="segmented" role="group" aria-label={m.show}>
    {tab('pending', m.showPending(list ? String(pending.length) : '…'))}
    {tab('all', m.showAll(props.info.latest.toString()))}
  </div>}>
    {proposals.error && <Banner tone="danger">{proposals.error}</Banner>}
    {!list
      ? !proposals.error && <p className="muted">{t.core.common.loading}</p>
      : items.length === 0
        ? <Empty>{filter === 'pending' && list.length > 0 ? m.noPending : m.noProposals}</Empty>
        : <ul className="multisig-proposals">{items.map((p) => <ProposalCard key={p.id.toString()} p={p} {...props} />)}</ul>}
    {proposals.data && !proposals.data.complete && <p className="field-hint">{m.partial(proposals.data.list.length.toLocaleString())}</p>}
    {pages > 1 && <div className="method-actions multisig-pager">
      <Button size="sm" disabled={at === 0} onClick={() => setPage(at - 1)}>{m.newer}</Button>
      <span className="muted">{m.pageOf(String(at + 1), String(pages))}</span>
      <Button size="sm" disabled={at >= pages - 1} onClick={() => setPage(at + 1)}>{m.older}</Button>
    </div>}
  </Section>;
}

/** When it was proposed, in the page's language; on an English page in the browser's own English (en-GB puts the day first) */
function dateOf(seconds: bigint, locale: string): string {
  const date = new Date(Number(seconds) * 1000);
  if (Number.isNaN(date.getTime())) return '';
  const own = typeof navigator === 'undefined' ? '' : navigator.language;
  for (const tag of [locale === 'en' && /^en(-|$)/i.test(own) ? own : locale, locale]) {
    try {
      return date.toLocaleString(tag);
    } catch {
      // a tag the browser reports but refuses (en-US@posix, say): the page's language instead
    }
  }
  return date.toLocaleString();
}

function BlockerText({ blocker }: { blocker: Blocker }) {
  const m = useT().multisig;
  switch (blocker.kind) {
    case 'approvals':
      return <p className="field-hint">{m.needsMore(blocker.missing)}</p>;
    case 'reading':
      return <p className="field-hint">{m.reading}</p>;
    case 'rule':
      return <p className="field-error">{m.problems[blocker.problem]} {m.stale}</p>;
    case 'balance':
      return <p className="field-error">
        <Words parts={m.short(<Amount value={blocker.held} asset={blocker.asset} />, <Amount value={blocker.needed} asset={blocker.asset} />)} />
      </p>;
  }
}

/**
 * What stands between a pending proposal and its execution, all of it at once (the contract stops at the first): an action the
 * owners and threshold of the moment refuse, then the approvals missing and the wallet's balance of what it takes
 */
function blockersOf(p: Proposal, info: WalletInfo, self: string, balances: ReadonlyMap<string, bigint> | null): Blocker[] {
  const problem = checkAction(p.action, info, self);
  if (problem) return [{ kind: 'rule', problem }];
  const out: Blocker[] = [];
  if (p.approvals.length < info.threshold) out.push({ kind: 'approvals', missing: info.threshold - p.approvals.length });
  const short = shortfall(p.action, balances);
  if (short) out.push(short);
  return out;
}

function ProposalCard({ contract, abi, info, me, balances, metas, p }: Props & { p: Proposal }) {
  const t = useT();
  const m = t.multisig;
  const conn = useConnection();
  const isContract = useKnownContracts();
  const locale = useLocale();
  const act = useAct();
  const id = p.id.toString();
  const pending = p.status === 'Pending';
  const isOwner = me !== null && info.owners.includes(me);
  const approved = me !== null && p.approvals.includes(me);
  const ready = pending && p.approvals.length >= info.threshold;
  const needsYou = pending && isOwner && !approved && !ready;
  const blockers = pending ? blockersOf(p, info, contract, balances) : [];
  const executable = pending && executeBlocker(p, info, contract, balances) === null;
  const run = (method: 'approve' | 'revoke' | 'execute' | 'cancel', title: string) => {
    const details: Detail[] = [
      { label: m.proposalLabel, value: `#${id}` },
      { label: m.actionLabel, value: actionText(m, p.action, metas, conn?.network, isContract) },
    ];
    void act.send(method, title, () => callAction(abi, contract, method, [p.id]), details);
  };
  const button = (method: 'approve' | 'revoke' | 'execute' | 'cancel', label: string, title: string,
    variant: 'primary' | 'secondary' | 'danger', disabled = false) =>
    <Button size="sm" variant={variant} disabled={act.busy || disabled} busy={act.which === method} onClick={() => run(method, title)}>{label}</Button>;
  const tone = p.status === 'Executed' ? 'pill--ok' : p.status === 'Cancelled' ? 'pill--quiet' : 'pill--info';
  const date = dateOf(p.createdAt, locale);
  return <li className={`multisig-proposal${needsYou ? ' multisig-proposal--you' : ''}`}>
    <div className="multisig-head">
      <span className="mono multisig-id">#{id}</span>
      <span className={`pill ${tone}`}>{m.status[p.status]}</span>
      {executable && <span className="pill pill--ok">{m.ready}</span>}
      {needsYou && <span className="pill pill--warn">{m.needsYou}</span>}
      {date && <span className="muted multisig-date">{date}</span>}
    </div>
    <p className="multisig-action"><ActionWords action={p.action} me={me} /></p>
    {p.action.kind === 'Call' && p.action.args.length > 0 && <details className="multisig-args">
      <summary>{m.argsLabel}</summary>
      <code className="mono">0x{toHex(p.action.args)}</code>
    </details>}
    <div className="multisig-line"><span className="muted">{m.proposedBy}</span><Who account={p.proposer} me={me} /></div>
    {pending && <div className="multisig-meter">
      <Meter value={info.threshold > 0 ? p.approvals.length / info.threshold : 1}
        label={m.approvalCount(String(p.approvals.length), String(info.threshold))} />
      <span className="field-hint">{m.approvalCount(String(p.approvals.length), String(info.threshold))}</span>
    </div>}
    <div className="multisig-line">
      <span className="muted">{m.approvedBy}</span>
      {p.approvals.length > 0 ? p.approvals.map((a) => <Who key={a} account={a} me={me} />) : <span className="muted">{m.nobody}</span>}
    </div>
    {blockers.map((b) => <BlockerText key={b.kind} blocker={b} />)}
    {/* an owner's buttons; without a wallet, the ones an owner would have, disabled (the page asks to connect once) */}
    {pending && (isOwner || me === null) && <div className="method-actions">
      {!approved && button('approve', m.approve, m.approveTitle(id), ready ? 'secondary' : 'primary', me === null)}
      {approved && button('revoke', m.revoke, m.revokeTitle(id), 'secondary')}
      {ready && button('execute', m.execute, m.executeTitle(id), 'primary', me === null || !executable)}
      {me !== null && p.proposer === me && button('cancel', m.cancel, m.cancelTitle(id), 'danger')}
    </div>}
    {act.error && <Banner tone="danger">{act.error}</Banner>}
  </li>;
}
