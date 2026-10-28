/** Looking up a borrower, and liquidating a position above its liquidation limit: what the liquidator pays, receives and gets back. */
import { useState } from 'react';
import { toHex } from '@ksana/sdk';
import { accountText } from '../../abi/text';
import { balanceOf, useAccount } from '../../chain/account';
import { useIsContract } from '../../chain/contracts';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useT } from '../../i18n';
import { Banner, Button, Rows, Section } from '../../ui/ui';
import { AccountField, accountOf, sameAccount } from '../../ui/values';
import { callAction, useSend } from '../kit';
import { closeLimit, collateralValue, coveredRepay, healthState, quoteLiquidation } from './math';
import { AmountInput, AmountView, HealthBadge, amountInput, eatsFee, positiveAmount, spendable, useLastGood } from './parts';
import { amountText, readPosition, shortAccount, type MarketContext, type Position } from './reads';

export function LiquidatePanel({ ctx }: { ctx: MarketContext }) {
  const m = useT().lending;
  const [text, setText] = useState('');
  const borrower = accountOf(text, ctx.network).account;
  return <Section title={m.liquidateTitle}>
    <div className="form-grid">
      <p className="field-hint">{m.liquidateHint}</p>
      <AccountField label={m.borrower} value={text} onChange={setText} />
      {borrower && <Borrower key={toHex(borrower)} ctx={ctx} borrower={borrower} />}
    </div>
  </Section>;
}

/** One borrower's position, read for that account alone (mounted again for every account typed) */
function Borrower({ ctx, borrower }: { ctx: MarketContext; borrower: Uint8Array }) {
  const t = useT();
  const m = t.lending;
  const { contract, abi, market, loan, coll, me } = ctx;
  const view = useView(contract, abi, 'position', [borrower]);
  const position = readPosition(useLastGood(view.data));
  if (!position) return view.error ? <Banner tone="danger">{view.error}</Banner> : <p className="muted">{t.core.common.loading}</p>;
  const state = healthState(position.debt, position.liquidationLimit);
  return <>
    <Rows rows={[
      { label: m.debt, value: <AmountView unit={loan} value={position.debt} /> },
      {
        label: m.collateral,
        value: <div className="lending-stack">
          <AmountView unit={coll} value={position.collateral} />
          {position.collateral > 0n && <span className="field-hint">{m.worth(amountText(loan, collateralValue(position.collateral, market.price)))}</span>}
        </div>,
      },
      { label: m.liquidationLimit, value: <AmountView unit={loan} value={position.liquidationLimit} /> },
      { label: m.health, value: <HealthBadge debt={position.debt} liquidationLimit={position.liquidationLimit} /> },
    ]} />
    {position.debt === 0n ? <p className="field-hint">{m.noDebtOther}</p>
      : state !== 'liquidatable' ? <p className="field-hint">{m.healthy}</p>
        : sameAccount(borrower, me) ? <Banner tone="warn">{m.selfLiquidation}</Banner>
          : <LiquidateForm ctx={ctx} borrower={borrower} position={position} />}
  </>;
}

function LiquidateForm({ ctx, borrower, position }: { ctx: MarketContext; borrower: Uint8Array; position: Position }) {
  const m = useT().lending;
  const borrowerIsContract = useIsContract(borrower);
  const wallet = useWallet();
  const account = useAccount();
  const sender = useSend();
  const [text, setText] = useState('');
  const { contract, abi, market, loan, coll, network } = ctx;
  const bonus = market.params.liquidationBonusBps;
  const balance = account ? balanceOf(account, loan.hex) : null;
  const most = closeLimit(position.debt);
  // past what the collateral covers, the liquidator pays more for no more collateral
  const covered = coveredRepay(position.collateral, market.price, bonus);
  const suggested = covered < most ? covered : most;
  const amount = positiveAmount(text, loan.decimals);
  const quote = amount !== null ? quoteLiquidation(amount, position.debt, position.collateral, market.price, bonus) : null;
  const problem = !wallet || amount === null ? null
    : balance !== null && amount > balance ? m.overBalance
      : quote !== null && quote.seized === 0n && position.collateral > 0n ? m.nothingSeized : null;
  const ready = !!wallet && quote !== null && quote.repaid > 0n && problem === null && amount !== null;
  const max = () => {
    if (balance === null) return;
    const most = spendable(loan, balance);
    setText(amountInput(suggested < most ? suggested : most, loan.decimals));
  };
  const send = () => {
    if (!ready || amount === null || quote === null) return;
    void sender.send({
      title: m.liquidateOp(amountText(loan, quote.repaid), shortAccount(borrower, network, borrowerIsContract)),
      action: callAction(abi, contract, 'liquidate', [borrower], [{ asset: loan.bytes, amount }]),
      details: [
        { label: m.borrower, value: accountText(borrower, network, borrowerIsContract) },
        { label: m.youRepay, value: amountText(loan, quote.repaid) },
        { label: m.youReceive, value: amountText(coll, quote.seized) },
      ],
    }).then((a) => { if (a) setText(''); });
  };
  return <div className="form-grid">
    <p className="field-hint">{m.mostRepay(amountText(loan, most))}</p>
    <AmountInput label={m.amount} unit={loan.name} decimals={loan.decimals} value={text} onChange={setText}
      onMax={wallet && balance !== null && spendable(loan, balance) > 0n && suggested > 0n ? max : null} problem={problem}
      hint={wallet ? <>{m.liquidateMaxHint(amountText(loan, balance ?? 0n))}{eatsFee(loan, amount, balance) && <> · {m.keepFee}</>}</> : undefined} />
    {quote !== null && amount !== null && problem === null && <div className="outcome">
      <Rows rows={[
        { label: m.youRepay, value: <AmountView unit={loan} value={quote.repaid} /> },
        { label: m.youReceive, value: m.receiveValue(amountText(coll, quote.seized), amountText(loan, collateralValue(quote.seized, market.price))) },
        quote.refund > 0n && { label: m.refund, value: <AmountView unit={loan} value={quote.refund} /> },
        quote.writtenOff > 0n && { label: m.writtenOff, value: m.writtenOffValue(amountText(loan, quote.writtenOff)) },
      ]} />
      {quote.capped && <Banner tone="warn">
        {m.capped(amountText(loan, collateralValue(position.collateral, market.price)), amountText(loan, quote.repaid))}
      </Banner>}
    </div>}
    <div className="method-actions">
      <Button variant="primary" disabled={!ready || sender.busy} busy={sender.busy} onClick={send}>{m.liquidate}</Button>
    </div>
    {sender.error && <Banner tone="danger">{sender.error}</Banner>}
  </div>;
}
