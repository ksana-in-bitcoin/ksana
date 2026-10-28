/** The admin's controls: setting the price in tokens, withdrawing reserves, and handing the role on or giving it up for good. */
import { useState } from 'react';
import { parseUnits } from '../../chain/assets';
import { useWallet } from '../../chain/wallet';
import { useT } from '../../i18n';
import { Banner, Button, Section } from '../../ui/ui';
import { AccountField, accountOf, sameAccount } from '../../ui/values';
import { callAction, useSend } from '../kit';
import { priceDecimals, priceFromTyped, priceText } from './math';
import { AmountInput, amountInput, positiveAmount } from './parts';
import { amountText, type MarketContext } from './reads';

export function AdminPanel({ ctx }: { ctx: MarketContext }) {
  const m = useT().lending;
  return <Section title={m.adminTitle}>
    <p className="field-hint">{m.adminIntro}</p>
    <h3 className="sub-title">{m.setPrice}</h3>
    <PriceForm ctx={ctx} />
    <h3 className="sub-title">{m.withdrawReserves}</h3>
    <ReservesForm ctx={ctx} />
    <h3 className="sub-title">{m.role}</h3>
    <RoleForm ctx={ctx} />
  </Section>;
}

function isZero(account: Uint8Array): boolean {
  return account.every((b) => b === 0);
}

function PriceForm({ ctx }: { ctx: MarketContext }) {
  const m = useT().lending;
  const sender = useSend();
  const [text, setText] = useState('');
  const { contract, abi, market, loan, coll } = ctx;
  // the price is typed as loan tokens for one collateral token, with as many decimals as the contract's price holds exactly
  const decimals = Math.max(priceDecimals(loan.decimals, coll.decimals), 0);
  let typed: bigint | null = null;
  if (text.trim()) {
    try {
      typed = parseUnits(text, decimals);
    } catch {
      typed = null;
    }
  }
  const price = typed !== null ? priceFromTyped(typed, loan.decimals, coll.decimals) : null;
  const words = (p: bigint) => m.priceValue(coll.name, priceText(p, loan.decimals, coll.decimals), loan.name);
  const problem = typed === null ? null
    : typed === 0n ? m.priceZero
      : price === null ? m.priceInvalid
        : price === market.price ? m.priceSame : null;
  const send = () => {
    if (price === null || problem !== null || !window.confirm(m.priceConfirm)) return;
    void sender.send({
      title: m.priceOp(words(price)),
      action: callAction(abi, contract, 'set_price', [price]),
      details: [{ label: m.price, value: words(price) }],
    }).then((a) => { if (a) setText(''); });
  };
  return <div className="form-grid">
    <AmountInput label={m.newPrice} unit={m.priceUnit(loan.name, coll.name)} decimals={decimals} value={text} onChange={setText}
      problem={problem} hint={m.priceNow(words(market.price))} />
    <div className="method-actions">
      <Button variant="primary" disabled={price === null || problem !== null || sender.busy} busy={sender.busy} onClick={send}>{m.setPriceButton}</Button>
    </div>
    {sender.error && <Banner tone="danger">{sender.error}</Banner>}
  </div>;
}

function ReservesForm({ ctx }: { ctx: MarketContext }) {
  const m = useT().lending;
  const wallet = useWallet();
  const sender = useSend();
  const [text, setText] = useState('');
  const [to, setTo] = useState(wallet?.account.address ?? '');
  const { contract, abi, market, loan, network } = ctx;
  // withdraw_reserves takes at most the reserves and at most the cash
  const most = market.reserves < market.cash ? market.reserves : market.cash;
  const amount = positiveAmount(text, loan.decimals);
  const target = accountOf(to, network).account;
  const blocker = most === 0n ? m.noReserves : null;
  const problem = blocker === null && amount !== null && amount > most ? m.overReserves(amountText(loan, most)) : null;
  const zero = target !== null && isZero(target);
  const ready = blocker === null && problem === null && amount !== null && target !== null && !zero;
  const send = () => {
    if (!ready || amount === null || target === null) return;
    void sender.send({
      title: m.reservesOp(amountText(loan, amount)),
      action: callAction(abi, contract, 'withdraw_reserves', [amount, target]),
      details: [{ label: m.to, value: to.trim() }],
    }).then((a) => { if (a) setText(''); });
  };
  return <div className="form-grid">
    <AmountInput label={m.amount} unit={loan.name} decimals={loan.decimals} value={text} onChange={setText}
      onMax={most > 0n ? () => setText(amountInput(most, loan.decimals)) : null} problem={problem}
      hint={blocker ?? m.reservesHint(amountText(loan, most))} />
    <AccountField label={m.to} value={to} onChange={setTo} />
    {zero && <p className="field-error">{m.zeroAccount}</p>}
    <div className="method-actions">
      <Button variant="primary" disabled={!ready || sender.busy} busy={sender.busy} onClick={send}>{m.withdrawReserves}</Button>
    </div>
    {sender.error && <Banner tone="danger">{sender.error}</Banner>}
  </div>;
}

function RoleForm({ ctx }: { ctx: MarketContext }) {
  const m = useT().lending;
  const sender = useSend();
  const [to, setTo] = useState('');
  const { contract, abi, market, network } = ctx;
  const target = accountOf(to, network).account;
  const problem = target === null ? null : isZero(target) ? m.zeroAccount : sameAccount(target, market.admin) ? m.sameAdmin : null;
  return <div className="form-grid">
    <AccountField label={m.newAdmin} value={to} onChange={setTo} />
    {problem && <p className="field-error">{problem}</p>}
    <div className="method-actions">
      <Button disabled={target === null || problem !== null || sender.busy} busy={sender.busy}
        onClick={() => target && problem === null && void sender.send({
          title: m.handOverOp,
          action: callAction(abi, contract, 'set_admin', [target]),
          details: [{ label: m.newAdmin, value: to.trim() }],
        }).then((a) => { if (a) setTo(''); })}>{m.handOver}</Button>
      <Button variant="danger" disabled={sender.busy}
        onClick={() => window.confirm(m.giveUpConfirm) && void sender.send({ title: m.giveUpOp, action: callAction(abi, contract, 'set_admin', [null]) })}>
        {m.giveUp}
      </Button>
    </div>
    {sender.error && <Banner tone="danger">{sender.error}</Banner>}
  </div>;
}
