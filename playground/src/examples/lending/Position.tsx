/** The connected account's part of the market: its position and health, then the panels to lend, deposit collateral and borrow. */
import { useMemo } from 'react';
import { balanceOf, useAccount } from '../../chain/account';
import { useView } from '../../chain/views';
import { useT } from '../../i18n';
import { Banner, Empty, Meter, Rows, Section, Stat } from '../../ui/ui';
import { BorrowPanel, CollateralPanel, LendPanel } from './Actions';
import { BPS, borrowRoom, collateralValue, healthBps, healthState, limitUsedBps, liquidity, percentText, priceDropBps } from './math';
import { AmountView, HealthBadge, useLastGood } from './parts';
import { amountText, readPosition, sharesText, type MarketContext, type Position } from './reads';

/** Mounted again for every wallet account (keyed by it), so nothing of another account's position is ever shown */
export function Personal({ ctx }: { ctx: MarketContext }) {
  const { contract, abi, me } = ctx;
  const view = useView(contract, abi, 'position', me ? [me] : [], { enabled: !!me });
  const kept = useLastGood(view.data);
  const position = useMemo(() => readPosition(kept), [kept]);
  return <>
    <PositionPanel ctx={ctx} position={position} error={view.error} unreadable={kept !== null && !position} />
    <LendPanel ctx={ctx} position={position} />
    <CollateralPanel ctx={ctx} position={position} />
    <BorrowPanel ctx={ctx} position={position} />
  </>;
}

function PositionPanel({ ctx, position, error, unreadable }: { ctx: MarketContext; position: Position | null; error: string | null; unreadable: boolean }) {
  const t = useT();
  const m = t.lending;
  const account = useAccount();
  const { market, loan, coll, me } = ctx;
  if (!me) return <Section title={m.you}><p className="field-hint">{t.core.errors.connectFirst}</p></Section>;
  if (!position) {
    return <Section title={m.you}>
      {error ? <Banner tone="danger">{error}</Banner> : unreadable ? <Banner tone="danger">{m.unreadable}</Banner> : <p className="muted">{t.core.common.loading}</p>}
    </Section>;
  }
  const p = position;
  const wallet = account && {
    label: m.inWallet,
    value: <div className="lending-stack"><AmountView unit={loan} value={balanceOf(account, loan.hex)} /><AmountView unit={coll} value={balanceOf(account, coll.hex)} /></div>,
  };
  if (p.shares === 0n && p.collateral === 0n && p.debt === 0n) {
    return <Section title={m.you}>
      <Empty>{m.nothingYet}</Empty>
      <Rows rows={[wallet]} />
    </Section>;
  }
  const state = healthState(p.debt, p.liquidationLimit);
  const used = limitUsedBps(p.debt, p.liquidationLimit);
  const drop = percentText(priceDropBps(healthBps(p.liquidationLimit, p.debt) ?? 0n));
  const hint = state === 'none' ? m.stateHints.none
    : state === 'liquidatable' ? m.stateHints.liquidatable
      : state === 'near' ? m.stateHints.near(drop) : m.stateHints.safe(drop);
  return <Section title={m.you}>
    <div className="form-grid">
      <div className="stats">
        <Stat label={m.supplied} value={<AmountView unit={loan} value={p.supplied} />}
          sub={p.shares > 0n ? `${m.sharesSub(sharesText(loan, p.shares))} · ${m.earning(percentText(market.supplyRateBps))}` : undefined} />
        <Stat label={m.collateral} value={<AmountView unit={coll} value={p.collateral} />}
          sub={p.collateral > 0n ? m.worth(amountText(loan, collateralValue(p.collateral, market.price))) : undefined} />
        <Stat label={m.debt} value={<AmountView unit={loan} value={p.debt} />}
          sub={p.debt > 0n ? m.atRate(percentText(market.borrowRateBps)) : undefined} />
      </div>
      {(p.debt > 0n || p.collateral > 0n) && <div className="lending-meter">
        <div className="kv"><span>{m.health}</span><HealthBadge debt={p.debt} liquidationLimit={p.liquidationLimit} /></div>
        <Meter value={Number(used) / Number(BPS)} tone={state === 'liquidatable' ? 'danger' : state === 'near' ? 'warn' : 'qing'} label={m.meterLabel} />
        <p className={state === 'liquidatable' ? 'field-error' : 'field-hint'}>{state === 'none' || state === 'liquidatable' ? hint : m.usedOfLimit(percentText(used), hint)}</p>
      </div>}
      <Rows rows={[
        { label: m.borrowLimit, value: <AmountView unit={loan} value={p.borrowLimit} /> },
        { label: m.liquidationLimit, value: <AmountView unit={loan} value={p.liquidationLimit} /> },
        { label: m.canBorrow, value: <AmountView unit={loan} value={borrowRoom(p.borrowLimit, p.debt, liquidity(market))} /> },
        wallet,
      ]} />
    </div>
  </Section>;
}
