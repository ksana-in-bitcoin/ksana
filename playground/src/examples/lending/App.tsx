/** The lending market: its assets, price and rates, your position, supplying, borrowing and repaying, liquidations, and the admin's controls. */
import { useMemo, type ReactNode } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import type { Value } from '../../abi/types';
import { requestDeploy } from '../../app/deploys';
import { useAssetMeta } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { assetInstead, useAssetProblem, type IdProblem } from '../../chain/ids';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useT, type Messages } from '../../i18n';
import { idProblemText } from '../../ui/errors';
import { ContractEvents, type ContractEvent } from '../../ui/Events';
import { Banner, Button, Id, Meter, Rows, Section, Stat } from '../../ui/ui';
import { sameAccount } from '../../ui/values';
import { HowItWorks } from '../Prose';
import type { AppProps } from '../kit';
import { AdminPanel } from './Admin';
import { RateCurve } from './Curve';
import { LiquidatePanel } from './Liquidate';
import { Personal } from './Position';
import { BPS, liquidity, percentText, priceText, rateCorners, totalAssets } from './math';
import { AmountView, Who, Words, useLastGood } from './parts';
import { readMarket, sharesText, unitOf, type MarketContext, type Unit } from './reads';
import './lending.css';

export default function App({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.lending;
  const conn = useConnection();
  const wallet = useWallet();
  const me = wallet ? fromHex(wallet.account.account, 32) : null;
  const view = useView(contract, abi, 'market');
  const kept = useLastGood(view.data);
  const market = useMemo(() => readMarket(kept), [kept]);
  const loanMeta = useAssetMeta(conn, market ? toHex(market.loanAsset) : null);
  const collMeta = useAssetMeta(conn, market ? toHex(market.collateralAsset) : null);
  const loanProblem = useAssetProblem(market ? toHex(market.loanAsset) : null);
  const collProblem = useAssetProblem(market ? toHex(market.collateralAsset) : null);
  if (!market || !loanMeta || !collMeta) {
    return <Section title={m.market}>
      {view.error ? <Banner tone="danger">{view.error}</Banner>
        : kept !== null && !market ? <Banner tone="danger">{m.unreadable}</Banner>
          : <p className="muted">{t.core.common.loading}</p>}
    </Section>;
  }
  const ctx: MarketContext = {
    contract, abi, market, me, network: conn?.network,
    loan: unitOf(market.loanAsset, loanMeta),
    coll: unitOf(market.collateralAsset, collMeta),
  };
  return <div className="app-grid">
    <div className="app-col">
      {view.error && <Banner tone="danger">{view.error}</Banner>}
      {(loanProblem || collProblem) && <BrokenMarket market={market} problems={[loanProblem, collProblem]} />}
      <MarketPanel ctx={ctx} />
      <Personal key={wallet?.account.account ?? ''} ctx={ctx} />
      <LiquidatePanel ctx={ctx} />
      {sameAccount(market.admin, me) && <AdminPanel ctx={ctx} />}
    </div>
    <div className="app-col">
      <ParamsPanel ctx={ctx} />
      <ContractEvents contract={contract} abi={abi} title={m.recent} render={(e) => eventSentence(e, ctx, m)} />
      <HowItWorks title={m.howTitle} items={m.how} />
    </div>
  </div>;
}

/**
 * A market one of whose assets names no asset, as a token contract's ID given for its collateral: what the ID names, and a new
 * market with the same price and parameters when it leads to an asset (the panels below stay, for whatever was supplied)
 */
function BrokenMarket({ market, problems }: { market: MarketContext['market']; problems: [IdProblem | null, IdProblem | null] }) {
  const t = useT();
  const m = t.lending;
  const labels = [m.loanAsset, m.collateralAsset];
  const assets = [market.loanAsset, market.collateralAsset];
  const mended = problems.map((problem, i) => {
    if (!problem) return { id: assets[i]!, name: null as string | null };
    const instead = assetInstead(problem);
    return instead ? { id: fromHex(instead.id, 32), name: instead.name } : null;
  });
  const names = mended.flatMap((x) => (x?.name ? [x.name] : []));
  const p = market.params;
  const redeploy = () => requestDeploy('lending', {
    loan_asset: mended[0]!.id,
    collateral_asset: mended[1]!.id,
    price: market.price,
    params: {
      collateral_factor_bps: p.collateralFactorBps, liquidation_threshold_bps: p.liquidationThresholdBps, liquidation_bonus_bps: p.liquidationBonusBps,
      reserve_factor_bps: p.reserveFactorBps, base_rate_bps: p.baseRateBps, slope1_bps: p.slope1Bps, kink_bps: p.kinkBps, slope2_bps: p.slope2Bps,
    },
  });
  return <Section>
    <div className="form-grid">
      <Banner tone="danger" title={m.brokenTitle}>
        {problems.map((problem, i) => (problem ? <p key={labels[i]}>{labels[i]}: {idProblemText(t.core.ids, problem)}</p> : null))}
        <p>{m.brokenFixed}</p>
      </Banner>
      {mended.every((x) => x !== null) && names.length > 0 && <div className="method-actions">
        <Button variant="primary" onClick={redeploy}>{m.deployInstead(names.join(', '))}</Button>
      </div>}
    </div>
  </Section>;
}

function AssetLine({ unit }: { unit: Unit }) {
  return <span className="id"><span className="unit">{unit.name}</span><Id value={unit.hex} /></span>;
}

/** The market as a whole: what is supplied and borrowed, the rates, the assets and the price, and who sets it */
function MarketPanel({ ctx }: { ctx: MarketContext }) {
  const m = useT().lending;
  const { market, loan, coll, me, network } = ctx;
  const utilization = market.utilizationBps;
  const aboveKink = utilization > market.params.kinkBps;
  return <Section title={m.title(loan.name, coll.name)}>
    <div className="form-grid">
      <div className="stats">
        <Stat label={m.supplied} value={<AmountView unit={loan} value={totalAssets(market)} />} sub={m.suppliedSub} />
        <Stat label={m.borrowed} value={<AmountView unit={loan} value={market.totalBorrows} />} sub={m.borrowedSub} />
        <Stat label={m.available} value={<AmountView unit={loan} value={liquidity(market)} />} sub={m.availableSub} />
      </div>
      <div className="lending-meter">
        <div className="kv"><span>{m.utilization}</span><span className="mono">{percentText(utilization)}</span></div>
        <Meter value={Number(utilization) / Number(BPS)} tone={aboveKink ? 'warn' : 'qing'} label={m.utilization} />
        <p className="field-hint">{aboveKink ? m.aboveKink(percentText(market.params.kinkBps)) : m.utilizationHint(percentText(utilization))}</p>
      </div>
      <div className="stats">
        <Stat label={m.borrowRate} value={percentText(market.borrowRateBps)} sub={m.perYear} />
        <Stat label={m.supplyRate} value={percentText(market.supplyRateBps)} sub={m.perYear} />
      </div>
      <Rows rows={[
        { label: m.loanAsset, value: <AssetLine unit={loan} /> },
        { label: m.collateralAsset, value: <AssetLine unit={coll} /> },
        { label: m.price, value: <span className="mono">{m.priceValue(coll.name, priceText(market.price, loan.decimals, coll.decimals), loan.name)}</span> },
        { label: m.admin, value: market.admin ? <Who account={market.admin} me={me} network={network} /> : <span className="muted">{m.noAdmin}</span> },
        { label: m.reserves, value: <div className="lending-stack"><AmountView unit={loan} value={market.reserves} /><span className="field-hint">{m.reservesSub}</span></div> },
        { label: m.collateralHeld, value: <AmountView unit={coll} value={market.totalCollateral} /> },
      ]} />
      {market.admin ? <Banner tone="warn" title={m.adminWarningTitle}>{m.adminWarning}</Banner> : <Banner tone="info">{m.frozen}</Banner>}
    </div>
  </Section>;
}

function Param({ value, hint }: { value: string; hint: string }) {
  return <div className="lending-stack"><span className="mono">{value}</span><span className="field-hint">{hint}</span></div>;
}

/** The parameters fixed at deployment, and the rate curve they make */
function ParamsPanel({ ctx }: { ctx: MarketContext }) {
  const m = useT().lending;
  const { market } = ctx;
  const p = market.params;
  const corners = rateCorners(p);
  const top = corners[corners.length - 1]?.rate ?? p.baseRateBps;
  return <Section title={m.params}>
    <Rows rows={[
      { label: m.collateralFactor, value: <Param value={percentText(p.collateralFactorBps)} hint={m.collateralFactorHint} /> },
      { label: m.threshold, value: <Param value={percentText(p.liquidationThresholdBps)} hint={m.thresholdHint} /> },
      { label: m.bonus, value: <Param value={percentText(p.liquidationBonusBps)} hint={m.bonusHint} /> },
      { label: m.reserveFactor, value: <Param value={percentText(p.reserveFactorBps)} hint={m.reserveFactorHint} /> },
      {
        label: m.interest,
        value: p.kinkBps < BPS
          ? m.interestValue(percentText(p.baseRateBps), percentText(p.slope1Bps), percentText(p.kinkBps), percentText(p.slope2Bps))
          : m.interestFlat(percentText(p.baseRateBps), percentText(p.slope1Bps)),
      },
    ]} />
    <h3 className="sub-title">{m.curveTitle}</h3>
    <RateCurve params={p} utilization={market.utilizationBps} borrowRate={market.borrowRateBps} supplyRate={market.supplyRateBps}
      text={{ ...m.curve, label: m.curve.label(percentText(p.baseRateBps), percentText(top)) }} />
  </Section>;
}

/** One of the contract's events as a sentence; null for one whose fields do not read as the ABI says (shown as plain fields) */
function eventSentence(e: ContractEvent, ctx: MarketContext, m: Messages['lending']): ReactNode | null {
  const { loan, coll, me, network } = ctx;
  const v = e.values;
  const who = (x: Value | undefined) => (x instanceof Uint8Array && x.length === 32 ? <Who account={x} me={me} network={network} /> : null);
  const inLoan = (x: Value | undefined) => (typeof x === 'bigint' ? <AmountView unit={loan} value={x} /> : null);
  const inColl = (x: Value | undefined) => (typeof x === 'bigint' ? <AmountView unit={coll} value={x} /> : null);
  const parts = ((): ReactNode[] | null => {
    switch (e.name) {
      case 'supply':
      case 'withdraw': {
        const account = who(v.account);
        const amount = inLoan(v.amount);
        if (!account || !amount || typeof v.shares !== 'bigint') return null;
        return (e.name === 'supply' ? m.events.supply : m.events.withdraw)(account, amount, sharesText(loan, v.shares));
      }
      case 'deposit_collateral':
      case 'withdraw_collateral': {
        const account = who(v.account);
        const amount = inColl(v.amount);
        if (!account || !amount) return null;
        return (e.name === 'deposit_collateral' ? m.events.deposit : m.events.takeBack)(account, amount);
      }
      case 'borrow': {
        const account = who(v.account);
        const amount = inLoan(v.amount);
        return account && amount ? m.events.borrow(account, amount) : null;
      }
      case 'repay': {
        const payer = who(v.payer);
        const borrower = who(v.borrower);
        const amount = inLoan(v.amount);
        if (!payer || !borrower || !amount) return null;
        return sameAccount(v.payer as Uint8Array, v.borrower as Uint8Array) ? m.events.repay(payer, amount) : m.events.repayFor(payer, amount, borrower);
      }
      case 'liquidate': {
        const liquidator = who(v.liquidator);
        const borrower = who(v.borrower);
        const repaid = inLoan(v.repaid);
        const seized = inColl(v.seized);
        return liquidator && borrower && repaid && seized ? m.events.liquidate(liquidator, borrower, repaid, seized) : null;
      }
      case 'write_off': {
        const borrower = who(v.borrower);
        const amount = inLoan(v.amount);
        return borrower && amount ? m.events.writeOff(borrower, amount) : null;
      }
      case 'price':
        return typeof v.price === 'bigint'
          ? m.events.price(m.priceValue(coll.name, priceText(v.price, loan.decimals, coll.decimals), loan.name))
          : null;
      case 'admin': {
        const previous = who(v.previous);
        const admin = who(v.admin);
        if (v.previous === null && admin) return m.events.opened(admin);
        if (previous && admin) return m.events.handedOver(previous, admin);
        if (previous && v.admin === null) return m.events.gaveUp(previous);
        return null;
      }
      case 'withdraw_reserves': {
        const to = who(v.to);
        const amount = inLoan(v.amount);
        return to && amount ? m.events.reserves(to, amount) : null;
      }
      default:
        return null;
    }
  })();
  return parts ? <p className="lending-event"><Words parts={parts} /></p> : null;
}
