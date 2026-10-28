/** The connected account's operations: supply and withdraw, deposit and take back collateral, borrow and repay (its own debt or another's). */
import { useState } from 'react';
import { toHex } from '@ksana/sdk';
import { accountText } from '../../abi/text';
import { balanceOf, useAccount } from '../../chain/account';
import { useIsContract } from '../../chain/contracts';
import type { Detail } from '../../chain/run';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useT } from '../../i18n';
import { Banner, Button, Section } from '../../ui/ui';
import { AccountField, AmountField, accountOf, sameAccount } from '../../ui/values';
import { callAction, useSend } from '../kit';
import {
  borrowRoom, debtWithMargin, freeCollateral, healthBps, healthText, limitsOf, liquidity, maxWithdrawShares, sharesForAmount, sharesValue,
  supplyShares, totalAssets, withdrawProblem,
} from './math';
import { AmountInput, Modes, amountInput, eatsFee, positiveAmount, spendable, useLastGood } from './parts';
import { amountText, readPosition, sharesText, shortAccount, type MarketContext, type Position, type Unit } from './reads';

type Props = { ctx: MarketContext; position: Position | null };

/** The wallet's balance of an asset, null while it loads */
function useBalance(unit: Unit): bigint | null {
  const account = useAccount();
  return account ? balanceOf(account, unit.hex) : null;
}

function SendError({ error }: { error: string | null }) {
  return error ? <Banner tone="danger">{error}</Banner> : null;
}

// ---------------------------------------------------------------------------
// Lending

export function LendPanel({ ctx, position }: Props) {
  const m = useT().lending;
  const [mode, setMode] = useState<'supply' | 'withdraw'>('supply');
  return <Section title={m.lend}>
    <div className="form-grid">
      <p className="field-hint">{m.lendHint(ctx.loan.name)}</p>
      <Modes label={m.lend} value={mode} onChange={setMode} options={[{ value: 'supply', label: m.supply }, { value: 'withdraw', label: m.withdraw }]} />
      {mode === 'supply' ? <SupplyForm ctx={ctx} /> : <WithdrawForm ctx={ctx} position={position} />}
    </div>
  </Section>;
}

function SupplyForm({ ctx }: { ctx: MarketContext }) {
  const m = useT().lending;
  const wallet = useWallet();
  const balance = useBalance(ctx.loan);
  const sender = useSend();
  const [text, setText] = useState('');
  const { contract, abi, market, loan } = ctx;
  const amount = positiveAmount(text, loan.decimals);
  const quote = amount !== null ? supplyShares(amount, market) : null;
  const shares = typeof quote === 'bigint' ? quote : null;
  const problem = !wallet || amount === null ? null
    : balance !== null && amount > balance ? m.overBalance
      : quote === 'insolvent' ? m.insolvent
        : quote === 'tooSmall' ? m.tooSmallSupply : null;
  const send = () => {
    if (amount === null || shares === null) return;
    void sender.send({
      title: m.supplyOp(amountText(loan, amount)),
      action: callAction(abi, contract, 'supply', [], [{ asset: loan.bytes, amount }]),
      details: [{ label: m.sharesLabel, value: sharesText(loan, shares) }],
    }).then((a) => { if (a) setText(''); });
  };
  return <div className="form-grid">
    <AmountField label={m.amount} asset={loan.hex} value={text} onChange={setText} max={wallet ? undefined : null} error={problem}
      hint={eatsFee(loan, amount, balance) ? m.keepFee : undefined} />
    {!problem && shares !== null && <p className="field-hint">{m.supplyGets(sharesText(loan, shares))}</p>}
    <div className="method-actions">
      <Button variant="primary" disabled={!wallet || shares === null || problem !== null || sender.busy} busy={sender.busy} onClick={send}>{m.supply}</Button>
    </div>
    <SendError error={sender.error} />
  </div>;
}

function WithdrawForm({ ctx, position }: Props) {
  const m = useT().lending;
  const wallet = useWallet();
  const sender = useSend();
  const [text, setText] = useState('');
  // Max asked for all the shares: the amount shown is what they are worth now, and they keep earning until the block
  const [all, setAll] = useState(false);
  const { contract, abi, market, loan } = ctx;
  const held = position?.shares ?? 0n;
  const supplied = position?.supplied ?? 0n;
  const typed = positiveAmount(text, loan.decimals);
  const shares = all ? held : typed !== null ? sharesForAmount(typed, held, market) : null;
  const payout = shares !== null ? sharesValue(shares, market) : null;
  const issue = shares !== null ? withdrawProblem(shares, held, market) : null;
  const insolvent = market.totalShares > 0n && totalAssets(market) === 0n;
  let problem: string | null = null;
  if (wallet && position && held > 0n && shares !== null) {
    if (insolvent || issue === 'insolvent') problem = m.insolvent;
    else if ((!all && typed !== null && typed > supplied) || issue === 'shares') problem = m.overSupplied(amountText(loan, supplied));
    else if (issue === 'liquidity') problem = m.overLiquidity(amountText(loan, liquidity(market)));
    else if (issue !== null) problem = m.tooSmallWithdraw;
  }
  const blocker = wallet && position && held === 0n ? m.nothingSupplied : wallet && insolvent ? m.insolvent : null;
  const ready = !!wallet && blocker === null && problem === null && shares !== null && shares > 0n && payout !== null && payout > 0n;
  const max = () => {
    const most = maxWithdrawShares(held, market);
    setAll(most === held);
    setText(amountInput(sharesValue(most, market), loan.decimals));
  };
  const send = () => {
    if (!ready || shares === null || payout === null) return;
    void sender.send({
      title: all ? m.withdrawAllOp(amountText(loan, payout)) : m.withdrawOp(amountText(loan, payout)),
      action: callAction(abi, contract, 'withdraw', [shares]),
      details: [{ label: m.sharesLabel, value: sharesText(loan, shares) }, { label: m.receive, value: m.about(amountText(loan, payout)) }],
    }).then((a) => { if (a) { setText(''); setAll(false); } });
  };
  return <div className="form-grid">
    <AmountInput label={m.amount} unit={loan.name} decimals={loan.decimals} value={text} onChange={(v) => { setAll(false); setText(v); }}
      onMax={wallet && held > 0n && !insolvent ? max : null} problem={problem}
      hint={blocker ?? (all ? m.allShares(sharesText(loan, held)) : position ? m.suppliedHint(amountText(loan, supplied)) : undefined)} />
    {ready && shares !== null && payout !== null && <p className="field-hint">{m.withdrawGets(sharesText(loan, shares), amountText(loan, payout))}</p>}
    <div className="method-actions">
      <Button variant="primary" disabled={!ready || sender.busy} busy={sender.busy} onClick={send}>{m.withdraw}</Button>
    </div>
    <SendError error={sender.error} />
  </div>;
}

// ---------------------------------------------------------------------------
// Collateral

export function CollateralPanel({ ctx, position }: Props) {
  const m = useT().lending;
  const [mode, setMode] = useState<'deposit' | 'takeBack'>('deposit');
  return <Section title={m.collateralTitle}>
    <div className="form-grid">
      <p className="field-hint">{m.collateralHint(ctx.coll.name)}</p>
      <Modes label={m.collateralTitle} value={mode} onChange={setMode}
        options={[{ value: 'deposit', label: m.deposit }, { value: 'takeBack', label: m.takeBack }]} />
      {mode === 'deposit' ? <DepositForm ctx={ctx} position={position} /> : <TakeBackForm ctx={ctx} position={position} />}
    </div>
  </Section>;
}

function DepositForm({ ctx, position }: Props) {
  const m = useT().lending;
  const wallet = useWallet();
  const sender = useSend();
  const [text, setText] = useState('');
  const { contract, abi, market, loan, coll } = ctx;
  const balance = useBalance(coll);
  const amount = positiveAmount(text, coll.decimals);
  const problem = wallet && amount !== null && balance !== null && amount > balance ? m.overBalance : null;
  const limit = amount !== null && position ? limitsOf(position.collateral + amount, market.price, market.params).borrowLimit : null;
  const send = () => {
    if (amount === null) return;
    void sender.send({
      title: m.depositOp(amountText(coll, amount)),
      action: callAction(abi, contract, 'deposit_collateral', [], [{ asset: coll.bytes, amount }]),
    }).then((a) => { if (a) setText(''); });
  };
  return <div className="form-grid">
    <AmountField label={m.amount} asset={coll.hex} value={text} onChange={setText} max={wallet ? undefined : null} error={problem}
      hint={eatsFee(coll, amount, balance) ? m.keepFee : undefined} />
    {!problem && limit !== null && <p className="field-hint">{m.limitAfter(amountText(loan, limit))}</p>}
    <div className="method-actions">
      <Button variant="primary" disabled={!wallet || amount === null || problem !== null || sender.busy} busy={sender.busy} onClick={send}>{m.deposit}</Button>
    </div>
    <SendError error={sender.error} />
  </div>;
}

function TakeBackForm({ ctx, position }: Props) {
  const m = useT().lending;
  const wallet = useWallet();
  const sender = useSend();
  const [text, setText] = useState('');
  const { contract, abi, market, loan, coll } = ctx;
  const collateral = position?.collateral ?? 0n;
  // what the debt may grow to before the operation lands
  const debt = debtWithMargin(position?.debt ?? 0n, market.borrowIndex, market.borrowRateBps);
  const free = freeCollateral(collateral, debt, market.price, market.params.collateralFactorBps);
  const amount = positiveAmount(text, coll.decimals);
  const blocker = wallet && position && collateral === 0n ? m.noCollateral : null;
  const problem = wallet && position && blocker === null && amount !== null && amount > free ? m.overFree(amountText(coll, free)) : null;
  const ready = !!wallet && !!position && blocker === null && problem === null && amount !== null;
  const limit = ready && amount !== null ? limitsOf(collateral - amount, market.price, market.params).borrowLimit : null;
  const send = () => {
    if (!ready || amount === null) return;
    void sender.send({
      title: m.takeBackOp(amountText(coll, amount)),
      action: callAction(abi, contract, 'withdraw_collateral', [amount]),
    }).then((a) => { if (a) setText(''); });
  };
  return <div className="form-grid">
    <AmountInput label={m.amount} unit={coll.name} decimals={coll.decimals} value={text} onChange={setText}
      onMax={wallet && free > 0n ? () => setText(amountInput(free, coll.decimals)) : null} problem={problem}
      hint={blocker ?? (position ? m.freeHint(amountText(coll, free)) : undefined)} />
    {limit !== null && <p className="field-hint">{m.limitAfter(amountText(loan, limit))}</p>}
    <div className="method-actions">
      <Button variant="primary" disabled={!ready || sender.busy} busy={sender.busy} onClick={send}>{m.takeBack}</Button>
    </div>
    <SendError error={sender.error} />
  </div>;
}

// ---------------------------------------------------------------------------
// Borrowing and repaying

export function BorrowPanel({ ctx, position }: Props) {
  const m = useT().lending;
  const [mode, setMode] = useState<'borrow' | 'repay'>('borrow');
  return <Section title={m.borrowTitle}>
    <div className="form-grid">
      <p className="field-hint">{m.borrowHint(ctx.loan.name)}</p>
      <Modes label={m.borrowTitle} value={mode} onChange={setMode} options={[{ value: 'borrow', label: m.borrow }, { value: 'repay', label: m.repay }]} />
      {mode === 'borrow' ? <BorrowForm ctx={ctx} position={position} /> : <RepayForm ctx={ctx} position={position} />}
    </div>
  </Section>;
}

function BorrowForm({ ctx, position }: Props) {
  const m = useT().lending;
  const wallet = useWallet();
  const sender = useSend();
  const [text, setText] = useState('');
  const { contract, abi, market, loan } = ctx;
  const available = liquidity(market);
  const debt = position?.debt ?? 0n;
  // room for the interest the debt adds before the operation lands
  const projected = debtWithMargin(debt, market.borrowIndex, market.borrowRateBps);
  const limitRoom = position && position.borrowLimit > projected ? position.borrowLimit - projected : 0n;
  const room = position ? borrowRoom(position.borrowLimit, projected, available) : 0n;
  const amount = positiveAmount(text, loan.decimals);
  const blocker = !wallet || !position ? null
    : position.collateral === 0n ? m.noCollateralYet
      : limitRoom === 0n ? m.atLimit
        : available === 0n ? m.overAvailable(amountText(loan, 0n)) : null;
  const problem = !wallet || !position || blocker !== null || amount === null ? null
    : amount > available ? m.overAvailable(amountText(loan, available))
      : amount > limitRoom ? m.overRoom(amountText(loan, limitRoom)) : null;
  const ready = !!wallet && !!position && blocker === null && problem === null && amount !== null;
  const after = ready && amount !== null && position ? debt + amount : null;
  const send = () => {
    if (!ready || amount === null) return;
    void sender.send({
      title: m.borrowOp(amountText(loan, amount)),
      action: callAction(abi, contract, 'borrow', [amount]),
    }).then((a) => { if (a) setText(''); });
  };
  return <div className="form-grid">
    <AmountInput label={m.amount} unit={loan.name} decimals={loan.decimals} value={text} onChange={setText}
      onMax={wallet && room > 0n ? () => setText(amountInput(room, loan.decimals)) : null} problem={problem}
      hint={blocker ?? (position ? (limitRoom <= available ? m.roomHint : m.roomCash)(amountText(loan, room)) : undefined)} />
    {after !== null && position && <p className="field-hint">
      {m.debtAfter(amountText(loan, after), healthText(healthBps(position.liquidationLimit, after) ?? 0n))}
    </p>}
    <div className="method-actions">
      <Button variant="primary" disabled={!ready || sender.busy} busy={sender.busy} onClick={send}>{m.borrow}</Button>
    </div>
    <SendError error={sender.error} />
  </div>;
}

function RepayForm({ ctx, position }: Props) {
  const m = useT().lending;
  const [whose, setWhose] = useState<'mine' | 'other'>('mine');
  const [text, setText] = useState('');
  const other = accountOf(text, ctx.network).account;
  return <div className="form-grid">
    <Modes label={m.whose} value={whose} onChange={setWhose} options={[{ value: 'mine', label: m.mine }, { value: 'other', label: m.other }]} />
    {whose === 'mine'
      ? <RepayAmount ctx={ctx} borrower={ctx.me} debt={position?.debt ?? null} />
      : <>
        <AccountField label={m.borrower} value={text} onChange={setText} />
        {other && <OtherDebt key={toHex(other)} ctx={ctx} borrower={other} />}
      </>}
  </div>;
}

/** Another account's debt, read for that account alone (mounted again for every account typed) */
function OtherDebt({ ctx, borrower }: { ctx: MarketContext; borrower: Uint8Array }) {
  const t = useT();
  const view = useView(ctx.contract, ctx.abi, 'position', [borrower]);
  const position = readPosition(useLastGood(view.data));
  if (!position) return view.error ? <Banner tone="danger">{view.error}</Banner> : <p className="muted">{t.core.common.loading}</p>;
  return <RepayAmount ctx={ctx} borrower={borrower} debt={position.debt} />;
}

function RepayAmount({ ctx, borrower, debt }: { ctx: MarketContext; borrower: Uint8Array | null; debt: bigint | null }) {
  const m = useT().lending;
  const borrowerIsContract = useIsContract(borrower);
  const wallet = useWallet();
  const sender = useSend();
  const [text, setText] = useState('');
  const { contract, abi, market, loan, me, network } = ctx;
  const balance = useBalance(loan);
  const own = sameAccount(borrower, me);
  const amount = positiveAmount(text, loan.decimals);
  const blocker = wallet && debt === 0n ? (own ? m.noDebt : m.noDebtOther) : null;
  const problem = wallet && blocker === null && amount !== null && balance !== null && amount > balance ? m.overBalance : null;
  const ready = !!wallet && borrower !== null && debt !== null && blocker === null && problem === null && amount !== null;
  // the debt grows every second: an amount covering an hour's interest on top of it repays it all, and the contract refunds the rest
  const all = debt !== null ? debtWithMargin(debt, market.borrowIndex, market.borrowRateBps) : null;
  const max = () => {
    if (all === null || balance === null) return;
    const most = spendable(loan, balance);
    setText(amountInput(all < most ? all : most, loan.decimals));
  };
  const send = () => {
    if (!ready || amount === null || borrower === null || debt === null || all === null) return;
    const details: Detail[] = [];
    if (!own) details.push({ label: m.borrower, value: accountText(borrower, network, borrowerIsContract) });
    details.push({ label: m.debtNow, value: amountText(loan, debt) });
    if (amount > debt) details.push({ label: m.refund, value: m.refundValue });
    void sender.send({
      title: !own ? m.repayForOp(amountText(loan, amount < debt ? amount : debt), shortAccount(borrower, network, borrowerIsContract))
        : amount >= all ? m.repayAllOp : m.repayOp(amountText(loan, amount)),
      action: callAction(abi, contract, 'repay', [borrower], [{ asset: loan.bytes, amount }]),
      details,
    }).then((a) => { if (a) setText(''); });
  };
  return <div className="form-grid">
    <AmountInput label={m.amount} unit={loan.name} decimals={loan.decimals} value={text} onChange={setText}
      onMax={wallet && debt !== null && debt > 0n && balance !== null && spendable(loan, balance) > 0n ? max : null} problem={problem}
      hint={blocker ?? (wallet ? <>{m.repayHint(amountText(loan, balance ?? 0n))}{eatsFee(loan, amount, balance) && <> · {m.keepFee}</>}</> : undefined)} />
    {ready && amount !== null && debt !== null && all !== null && <p className="field-hint">
      {amount >= all ? m.repaysAll : amount >= debt ? m.interestLeft : m.debtLeft(amountText(loan, debt - amount))}
    </p>}
    <div className="method-actions">
      <Button variant="primary" disabled={!ready || sender.busy} busy={sender.busy} onClick={send}>{m.repay}</Button>
    </div>
    <SendError error={sender.error} />
  </div>;
}
