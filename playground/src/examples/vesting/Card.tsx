/** One vesting schedule: what it holds, has paid and has vested, its dates, and the release and revoke its parties can send. */
import { Fragment, type ReactNode } from 'react';
import { toHex } from '@ksana/sdk';
import type { Abi } from '../../abi/types';
import { formatUnits, useAssetMeta } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { useWallet } from '../../chain/wallet';
import { useLocale, useT, type Messages } from '../../i18n';
import { Banner, Button, Rows, Stat } from '../../ui/ui';
import { AccountText, Amount, AssetName, sameAccount } from '../../ui/values';
import { callAction, useSend } from '../kit';
import { VestingChart } from './Chart';
import {
  cliffAt, dateLocale, durationParts, endAt, formatDateTime, formatTick, percentText, phaseAt, releasableAt, share, splitAt, vestedAt,
  type Phase, type Schedule,
} from './schedule';

/** A length of time in words */
export function durationText(m: Messages['vesting'], seconds: bigint): string {
  const parts = durationParts(seconds);
  return parts.length ? parts.map(([unit, n]) => m.units[unit](n.toString())).join(' ') : m.noCliff;
}

/** Text with values in it, laid out in the order the language puts them */
export function Sentence({ parts, className }: { parts: ReactNode[]; className?: string }) {
  return <p className={className}>{parts.map((p, i) => <Fragment key={i}>{p}</Fragment>)}</p>;
}

const PHASE_TONE: Record<Phase, string> = {
  waiting: 'pill', cliff: 'pill', vesting: 'pill pill--ok', vested: 'pill pill--ok', done: 'pill pill--quiet', revoked: 'pill pill--quiet',
};

/** The bar of what was released and what has vested against the total */
export function VestingBar({ released, vested, total, label }: { released: bigint; vested: bigint; total: bigint; label: string }) {
  const r = share(released, total);
  const v = Math.max(r, share(vested, total));
  return <div className="vesting-bar" role="img" aria-label={label}>
    {r > 0 && <span className="vesting-bar-released" style={{ width: `${r * 100}%` }} />}
    {v > r && <span className={`vesting-bar-vested${r > 0 ? ' vesting-bar-vested--after' : ''}`} style={{ left: `${r * 100}%`, width: `${(v - r) * 100}%` }} />}
  </div>;
}

export function ScheduleCard({ contract, abi, schedule: s, now, releasable, me, chart = false }: {
  contract: string;
  abi: Abi;
  schedule: Schedule;
  /** The chain's time */
  now: bigint;
  /** `releasable(id)` as the contract answered it at the latest block; counted from `now` while it is not known */
  releasable: bigint | undefined;
  me: Uint8Array | null;
  /** Draws the schedule over time too (not once revoked: its total is then what had vested) */
  chart?: boolean;
}) {
  const t = useT();
  const m = t.vesting;
  const conn = useConnection();
  const wallet = useWallet();
  const locale = dateLocale(useLocale());
  const asset = toHex(s.asset);
  const meta = useAssetMeta(conn, asset);
  const releaser = useSend();
  const revoker = useSend();
  const id = s.id.toString();
  // What the contract counts at the latest block: what was released and what a release would pay now
  const due = releasable ?? releasableAt(s, now);
  const vested = releasable !== undefined ? s.released + releasable : vestedAt(s, now);
  const unvested = s.total > vested ? s.total - vested : 0n;
  const phase = phaseAt(s, now);
  const mine = sameAccount(s.beneficiary, me);
  const created = sameAccount(s.creator, me);
  const revocable = s.revocable && s.revoked_at === null;
  // Counted at the latest block; the block that includes a revoke counts a little more as vested, never less
  const split = splitAt(s, vested);
  const text = (v: bigint) => (meta ? `${formatUnits(v, meta.decimals)} ${meta.name}` : null);
  const date = (at: bigint) => formatDateTime(at, locale);
  const you = (account: Uint8Array) => (sameAccount(account, me) ? <span className="pill pill--ok">{t.core.common.you}</span> : null);
  const address = (account: Uint8Array) => <span className="id"><AccountText value={account} network={conn?.network} />{you(account)}</span>;

  const release = () => {
    const amount = text(due);
    if (!amount) return;
    void releaser.send({
      // While it vests, the block that includes the release pays a little more than the latest block counts
      title: m.releaseTitle(amount, id, phase === 'vesting'),
      action: callAction(abi, contract, 'release', [s.id]),
      details: [{ label: m.releasable, value: amount }],
    });
  };
  const revoke = () => {
    const { kept, refund, owed } = split;
    const [refundText, keptText, owedText] = [text(refund), text(kept), text(owed)];
    if (refund === 0n || !refundText || !keptText || !owedText) return;
    // What is still to be released is worth saying only when part of what has vested was released already
    if (!window.confirm(m.revokeConfirm(id, refundText, kept > 0n ? keptText : null, owed > 0n && owed < kept ? owedText : null))) return;
    void revoker.send({
      title: m.revokeTitle(id),
      action: callAction(abi, contract, 'revoke', [s.id]),
      details: [{ label: m.refund, value: refundText }, { label: m.keeps, value: keptText }],
    });
  };

  const cliffRow = s.cliff > 0n ? m.afterStart(date(cliffAt(s)), durationText(m, s.cliff)) : m.noCliff;
  return <article className="vesting-card">
    <div className="vesting-card-head">
      <h3 className="vesting-card-title">{m.schedule(id)} · <AssetName asset={asset} /></h3>
      <span className={PHASE_TONE[phase]}>{m.phases[phase]}</span>
    </div>
    <div className="vesting-due">
      <Stat label={m.releasable} value={<Amount value={due} asset={asset} />}
        sub={phase === 'waiting' || phase === 'cliff' ? m.nothingBefore(date(cliffAt(s))) : undefined} />
      {due > 0n && <Button variant={mine ? 'primary' : 'secondary'} size="sm" disabled={!wallet || !meta || releaser.busy} busy={releaser.busy} onClick={release}>
        {mine ? m.release : m.releaseFor}
      </Button>}
    </div>
    <VestingBar released={s.released} vested={vested} total={s.total} label={m.barLabel(percentText(s.released, s.total), percentText(vested, s.total))} />
    <ul className="vesting-key">
      <li><i className="vesting-swatch vesting-swatch--released" aria-hidden="true" />{m.released} <Amount value={s.released} asset={asset} /></li>
      <li><i className="vesting-swatch vesting-swatch--vested" aria-hidden="true" />{m.vestedNow} <Amount value={vested} asset={asset} /></li>
      {s.revoked_at === null && <li><i className="vesting-swatch" aria-hidden="true" />{m.unvested} <Amount value={unvested} asset={asset} /></li>}
    </ul>
    {chart && s.revoked_at === null && <VestingChart start={s.start} cliff={s.cliff} duration={s.duration} now={now} text={{
      label: m.chartLabel, vested: m.keys.vested, cliff: m.keys.cliff, now: m.keys.now, full: m.percent('100'),
      tick: (at) => formatTick(at, locale, s.duration),
      readout: (at) => m.readout(date(at), `${text(vestedAt(s, at)) ?? '…'} · ${m.percent(percentText(vestedAt(s, at), s.total))}`),
    }} />}
    <Rows rows={[
      { label: s.revoked_at !== null ? m.totalRevoked : m.total, value: <Amount value={s.total} asset={asset} /> },
      { label: m.beneficiary, value: address(s.beneficiary) },
      { label: m.creator, value: address(s.creator) },
      { label: m.start, value: date(s.start) },
      { label: m.cliffDate, value: cliffRow },
      { label: m.end, value: m.afterStart(date(endAt(s)), durationText(m, s.duration)) },
      { label: m.revocable, value: s.revoked_at !== null ? m.revokedOn(date(s.revoked_at)) : s.revocable ? m.revocableYes : m.revocableNo },
    ]} />
    {due > 0n && <p className="field-hint">{m.releaseHint}</p>}
    {wallet && created && revocable && <div className="method-actions">
      <Button variant="danger" size="sm" disabled={!meta || split.refund === 0n || revoker.busy} busy={revoker.busy} onClick={revoke}>{m.revoke}</Button>
      <span className="field-hint">{split.refund === 0n ? m.revokeNothing : m.revokeHint}</span>
    </div>}
    {releaser.error && <Banner tone="danger">{releaser.error}</Banner>}
    {revoker.error && <Banner tone="danger">{revoker.error}</Banner>}
  </article>;
}
