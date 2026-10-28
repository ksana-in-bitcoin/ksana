/**
 * The pool's three actions, swapping, adding liquidity and removing it, each quoted with the contract's own integer formulas. A
 * KS-20 side pays the pool in one operation, through its token's `transfer_and_call`; a token side before KS-20 is approved,
 * then pulled.
 */
import { useState, type ReactNode } from 'react';
import type { Action, Funds } from '@ksana/sdk';
import { decodeReturn } from '../../abi/codec';
import type { Abi, Value } from '../../abi/types';
import { navigate } from '../../app/route';
import { KSA_HEX, formatUnits } from '../../chain/assets';
import type { Activity } from '../../chain/run';
import { useChainTime } from '../../chain/time';
import { useWallet } from '../../chain/wallet';
import { useT } from '../../i18n';
import { useExpiryText } from '../../ui/expiry';
import { Banner, Button, Rows, Section } from '../../ui/ui';
import { AmountField, UnitsField, amountOf } from '../../ui/values';
import { callAction, useSend, type SendState } from '../kit';
import { Choice } from './Choice';
import { bpsText, deposit, matchingAmount, parsePercent, percentText, priceImpact, swapOutput, unitPrice, withSlippage, withdraw } from './math';
import { amountText, inputText, type Pool, type PoolSide } from './pool';
import { APPROVAL_SECONDS, answerOf, approvalNeeded, approveAction, attached, sendAction, type PoolAction } from './token';

type Mode = 'swap' | 'add' | 'remove';

/** The slippage tolerance: a preset in basis points, or (preset null) the percentage typed */
interface Slippage {
  preset: bigint | null;
  custom: string;
}

const PRESETS = [10n, 50n, 100n];
const MILLION = 1_000_000n;

function slippageBps(s: Slippage): bigint | null {
  return s.preset ?? parsePercent(s.custom);
}

/** Why a form cannot be sent yet: a hint while something is missing, an error when what was typed cannot work, or `shown` when a field already says why */
type Reason = { tone: 'hint' | 'error'; text: string } | 'shown' | null;

interface FormProps {
  contract: string;
  abi: Abi;
  pool: Pool;
  slippage: Slippage;
  onSlippage: (s: Slippage) => void;
  hidden: boolean;
}

/** The value a method returned in the operation's receipt, decoded; null when there is none */
function outputOf(abi: Abi, method: string, activity: Activity): Value | null {
  const receipt = activity.status?.receipt;
  const def = abi.methods.find((x) => x.name === method);
  if (!receipt || !def) return null;
  try {
    return decodeReturn(abi, def.returns, receipt.output);
  } catch {
    return null;
  }
}

/** The call, or the reason it cannot be built (an ABI that does not match the page) */
function buildCall(abi: Abi, contract: string, method: string, args: Value[], funds: Funds[] = []): Action | string {
  try {
    return callAction(abi, contract, method, args, funds);
  } catch (e) {
    return e instanceof Error ? e.message : String(e);
  }
}

/** A KS-20 side's `transfer_and_call` to the pool with `action`, or the reason it cannot be built */
function buildSend(side: PoolSide, pool: string, amount: bigint, action: PoolAction, funds: Funds[] = []): Action | string {
  try {
    return sendAction(side, pool, amount, action, funds);
  } catch (e) {
    return e instanceof Error ? e.message : String(e);
  }
}

/** The pool's answer to a `transfer_and_call` in the operation's receipt: the amount paid out or the shares */
function answerIn(activity: Activity): bigint | null {
  const receipt = activity.status?.receipt;
  return receipt ? answerOf(receipt.output) : null;
}

export function TradePanel({ contract, abi, pool, mine }: { contract: string; abi: Abi; pool: Pool; mine: bigint | null }) {
  const t = useT();
  const m = t.amm;
  const wallet = useWallet();
  const [mode, setMode] = useState<Mode>(pool.reserveA === 0n || pool.reserveB === 0n ? 'add' : 'swap');
  const [slippage, setSlippage] = useState<Slippage>({ preset: 50n, custom: '' });
  const shared = { contract, abi, pool, slippage, onSlippage: setSlippage };
  return <Section>
    <div className="form-grid">
      <Choice className="amm-modes" label={m.actions} value={mode} onChange={setMode}
        options={[{ value: 'swap', label: m.swap }, { value: 'add', label: m.add }, { value: 'remove', label: m.remove }]} />
      <SwapForm {...shared} hidden={mode !== 'swap'} onAdd={() => setMode('add')} />
      <AddForm {...shared} hidden={mode !== 'add'} />
      <RemoveForm {...shared} hidden={mode !== 'remove'} mine={mine} />
      {!wallet && <p className="field-hint">{t.core.errors.connectFirst}</p>}
    </div>
  </Section>;
}

function SlippageField({ value, onChange }: { value: Slippage; onChange: (s: Slippage) => void }) {
  const m = useT().amm;
  const bps = slippageBps(value);
  const options = [...PRESETS.map((p) => ({ value: p.toString(), label: `${bpsText(p)}%` })), { value: 'custom', label: m.custom }];
  const choose = (v: string) => {
    const preset = PRESETS.find((p) => p.toString() === v);
    onChange(preset !== undefined ? { preset, custom: value.custom } : { preset: null, custom: value.custom || bpsText(value.preset ?? 50n) });
  };
  return <div className="vfield">
    <span className="field-label">{m.slippage}</span>
    <div className="amm-slippage">
      <Choice label={m.slippage} value={value.preset === null ? 'custom' : value.preset.toString()} options={options} onChange={choose} />
      {value.preset === null && <>
        <input className="input mono" inputMode="decimal" autoComplete="off" aria-label={m.customLabel} value={value.custom}
          onChange={(e) => onChange({ preset: null, custom: e.target.value })} />
        <span className="unit">%</span>
      </>}
    </div>
    {bps === null ? <span className="field-error">{m.badSlippage}</span> : <span className="field-hint">{m.slippageHint}</span>}
  </div>;
}

/** Where to get a side the wallet holds none of: KSA from the faucet, another asset from the Asset example, a token by swapping or from its page */
function GetIt({ side }: { side: PoolSide }) {
  const m = useT().amm;
  if (side.kind === 'Token' && side.ks20) {
    return <>{m.getToken} · <button type="button" className="link-btn" onClick={() => navigate({ example: 'ks20', tab: 'app', contract: side.hex })}>{m.tokenPage}</button></>;
  }
  if (side.kind === 'Token') return <>{m.getToken}</>;
  if (side.hex === KSA_HEX) return <>{m.getKsa}</>;
  return <button type="button" className="link-btn" onClick={() => navigate({ example: 'token', tab: 'app', contract: null })}>{m.getAsset}</button>;
}

/**
 * An amount of one side with what the wallet holds of it, and an error when it is more than that: a built-in asset in its own
 * units, a token contract in the units its metadata gives, with what the pool may already pull of it when it is pulled
 */
function SideInput({ label, side, value, onChange, amount, pulled }: {
  label: ReactNode; side: PoolSide; value: string; onChange: (v: string) => void; amount: bigint | null; pulled: boolean;
}) {
  const t = useT();
  const m = t.amm;
  const wallet = useWallet();
  const balance = side.held;
  const over = balance !== null && amount !== null && amount > balance;
  const hint = balance === 0n ? <GetIt side={side} />
    : side.hex === KSA_HEX && amount !== null && amount > 0n && amount === balance ? m.keepFee : undefined;
  if (side.kind === 'Token') {
    const decimals = side.decimals ?? 0;
    const facts = [
      balance !== null ? `${t.core.wallet.balance}: ${amountText(side, balance)}` : null,
      pulled && side.allowance !== null ? m.allowance(amountText(side, side.allowance)) : null,
    ].filter(Boolean).join(' · ');
    // more than the balance is the field's own error
    return <div className="amm-field">
      <UnitsField label={label} unit={side.name} value={value} onChange={onChange} decimals={decimals} max={balance}
        hint={facts || hint ? <>{facts}{facts && hint ? ' · ' : ''}{hint}</> : undefined} />
    </div>;
  }
  return <div className="amm-field">
    <AmountField label={label} asset={side.hex} value={value} onChange={onChange} max={wallet ? undefined : null} hint={hint} />
    {over && <span className="field-error">{m.overBalance}</span>}
  </div>;
}

/** Lets the pool pull `amount` of a token side: `approve` on the token contract, through the review; a KS-20 token's for a week */
function ApproveButton({ approver, pool, side, amount }: { approver: SendState; pool: string; side: PoolSide; amount: bigint }) {
  const m = useT().amm;
  const time = useChainTime();
  const expiryText = useExpiryText();
  const text = amountText(side, amount);
  const until = time.now + APPROVAL_SECONDS;
  return <Button variant="primary" disabled={approver.busy} busy={approver.busy} onClick={() => void approver.send({
    title: m.approveTitle(text),
    action: approveAction(side, pool, amount, until),
    details: [
      { label: m.approveSpender, value: `${pool.slice(0, 8)}…` },
      ...(side.ks20 ? [{ label: m.approveExpires, value: expiryText(until, time.now) }] : []),
    ],
  })}>{m.approveButton(text)}</Button>;
}

function ReasonLine({ reason }: { reason: Reason }) {
  if (reason === null || reason === 'shown') return null;
  return <p className={reason.tone === 'error' ? 'field-error' : 'field-hint'}>{reason.text}</p>;
}

function impactTone(ppm: bigint): string {
  if (ppm >= 50_000n) return 'pill--danger';
  if (ppm >= 10_000n) return 'pill--warn';
  return 'pill--quiet';
}

function SwapForm({ contract, abi, pool, slippage, onSlippage, hidden, onAdd }: FormProps & { onAdd: () => void }) {
  const t = useT();
  const m = t.amm;
  const wallet = useWallet();
  const sender = useSend();
  const approver = useSend();
  const [dir, setDir] = useState<'ab' | 'ba'>('ab');
  const [text, setText] = useState('');
  const [done, setDone] = useState<string | null>(null);
  const [local, setLocal] = useState<string | null>(null);
  const ab = dir === 'ab';
  const from = ab ? pool.a : pool.b;
  const to = ab ? pool.b : pool.a;
  const reserveIn = ab ? pool.reserveA : pool.reserveB;
  const reserveOut = ab ? pool.reserveB : pool.reserveA;
  const empty = reserveIn === 0n || reserveOut === 0n;
  const bps = slippageBps(slippage);
  const amountIn = amountOf(text, from.decimals ?? undefined);
  const balance = from.held;
  const out = amountIn !== null && amountIn > 0n && !empty ? swapOutput(amountIn, reserveIn, reserveOut) : null;
  const minOut = out !== null && bps !== null ? withSlippage(out, bps) : null;
  const impact = amountIn !== null && out !== null ? priceImpact(amountIn, out, reserveIn, reserveOut) : null;
  // a KS-20 side pays with its token's transfer_and_call, which swaps in the same operation; one before KS-20 is approved, then pulled
  const oneStep = from.kind === 'Token' && from.ks20;
  const pulled = from.kind === 'Token' && !oneStep;
  const approval = pulled ? approvalNeeded(from, amountIn) : null;

  // what stands in the way of the swap besides an approval, which is offered only once nothing else does
  const blocked = ((): Reason => {
    if (from.decimals === null || to.decimals === null) return { tone: 'hint', text: t.core.common.loading };
    if (!text.trim()) return { tone: 'hint', text: m.enterPay };
    if (amountIn === null) return 'shown';
    if (amountIn === 0n) return { tone: 'error', text: m.zero };
    if (balance !== null && amountIn > balance) return 'shown';
    if (out === null) return { tone: 'error', text: m.tooLarge };
    if (out === 0n) return { tone: 'error', text: m.tooSmall };
    if (bps === null) return 'shown';
    return null;
  })();
  const allowanceLoads = !!wallet && pulled && from.allowance === null;
  const approve = blocked === null && !allowanceLoads ? approval : null;
  const reason: Reason = blocked ?? (allowanceLoads ? { tone: 'hint', text: t.core.common.loading }
    : approve !== null ? { tone: 'hint', text: m.approveFirst(from.name) } : null);

  const clear = () => {
    setDone(null);
    setLocal(null);
    sender.clear();
  };
  const flip = (next: 'ab' | 'ba') => {
    setDir(next);
    setText('');
    clear();
  };
  const send = () => {
    if (!wallet || amountIn === null || out === null || minOut === null || bps === null) return;
    const action = oneStep ? buildSend(from, contract, amountIn, { variant: 'Swap', values: [minOut] })
      : buildCall(abi, contract, 'swap', [from.id, amountIn, minOut], attached([[from, amountIn]]));
    if (typeof action === 'string') {
      setLocal(action);
      return;
    }
    const pay = amountText(from, amountIn);
    void sender.send({
      title: m.swapTitle(pay, amountText(to, out)),
      action,
      details: [
        ...(oneStep ? [{ label: m.sentWith, value: m.viaToken(from.name) }] : []),
        { label: m.youPay, value: pay },
        { label: m.minReceived, value: amountText(to, minOut) },
        { label: m.slippage, value: `${bpsText(bps)}%` },
      ],
    }).then((activity) => {
      if (!activity) return;
      setText('');
      const got = oneStep ? answerIn(activity) : outputOf(abi, 'swap', activity);
      setDone(typeof got === 'bigint' ? m.swapDone(amountText(to, got)) : null);
    });
  };

  const rate = amountIn !== null && out !== null && from.decimals !== null && to.decimals !== null
    ? unitPrice(amountIn, out, from.decimals, to.decimals) : null;
  return <div className="form-grid" hidden={hidden}>
    <Choice className="amm-start" label={m.direction} value={dir} onChange={flip}
      options={[{ value: 'ab', label: `${pool.a.name} → ${pool.b.name}` }, { value: 'ba', label: `${pool.b.name} → ${pool.a.name}` }]} />
    {empty ? <>
      <p className="field-hint">{m.emptyPool}</p>
      <div className="method-actions"><Button onClick={onAdd}>{m.add}</Button></div>
    </> : <>
      <SideInput label={m.youPay} side={from} value={text} onChange={(v) => { setText(v); clear(); }} amount={amountIn} pulled={pulled} />
      {oneStep && <p className="field-hint">{m.oneStepSwap(from.name)}</p>}
      {amountIn !== null && out !== null && out > 0n && <div className="outcome">
        <span className="outcome-label">{m.youGet}</span>
        <span className="amm-figure">{amountText(to, out)}</span>
        {minOut !== null && <span className="field-hint">{m.atLeast(amountText(to, minOut))}</span>}
        {to.baseUnits && <span className="field-hint">{m.baseUnits}</span>}
        <Rows rows={[
          { label: m.rate, value: <span className="mono">1 {from.name} ≈ {rate ?? '…'} {to.name}</span> },
          impact !== null && { label: m.priceImpact, value: <span className={`pill ${impactTone(impact)}`}>{percentText(impact, MILLION)}</span> },
          { label: m.swapFee, value: <span className="mono">{amountText(from, (amountIn * 3n) / 1000n)}</span> },
        ]} />
        {impact !== null && impact >= 50_000n && <p className="field-error">{m.impactHigh}</p>}
      </div>}
      <SlippageField value={slippage} onChange={onSlippage} />
      <ReasonLine reason={reason} />
      <div className="method-actions">
        {wallet && approve !== null && <ApproveButton approver={approver} pool={contract} side={from} amount={approve} />}
        <Button variant="primary" disabled={!wallet || reason !== null || sender.busy} busy={sender.busy} onClick={send}>{m.swapButton}</Button>
      </div>
      {approver.error && <Banner tone="danger">{approver.error}</Banner>}
      {done && <Banner tone="ok">{done}</Banner>}
      {(local ?? sender.error) && <Banner tone="danger">{local ?? sender.error}</Banner>}
    </>}
  </div>;
}

/** The two amounts of a deposit: in an empty pool both as typed; otherwise the one typed last, and the other at the pool's ratio */
function depositAmounts(pool: Pool, anchor: 'a' | 'b', typedA: bigint | null, typedB: bigint | null): { a: bigint | null; b: bigint | null; filled: boolean } {
  const { reserveA, reserveB, totalShares } = pool;
  if (totalShares === 0n || reserveA === 0n || reserveB === 0n) return { a: typedA, b: typedB, filled: false };
  if (anchor === 'a') return { a: typedA, b: typedA !== null && typedA > 0n ? matchingAmount(typedA, reserveA, reserveB, totalShares) : null, filled: true };
  return { a: typedB !== null && typedB > 0n ? matchingAmount(typedB, reserveB, reserveA, totalShares) : null, b: typedB, filled: true };
}

function AddForm({ contract, abi, pool, slippage, onSlippage, hidden }: FormProps) {
  const t = useT();
  const m = t.amm;
  const wallet = useWallet();
  const sender = useSend();
  const approver = useSend();
  const [anchor, setAnchor] = useState<'a' | 'b'>('a');
  const [textA, setTextA] = useState('');
  const [textB, setTextB] = useState('');
  const [done, setDone] = useState<string | null>(null);
  const [local, setLocal] = useState<string | null>(null);
  const { a, b, totalShares } = pool;
  const bps = slippageBps(slippage);
  const typedA = amountOf(textA, a.decimals ?? undefined);
  const typedB = amountOf(textB, b.decimals ?? undefined);
  const { a: amountA, b: amountB, filled } = depositAmounts(pool, anchor, typedA, typedB);
  // the side filled in at the ratio shows the amount worked out, not what was typed there before
  const shownA = filled && anchor === 'b' ? (amountA !== null && amountA > 0n && a.decimals !== null ? inputText(amountA, a.decimals) : '') : textA;
  const shownB = filled && anchor === 'a' ? (amountB !== null && amountB > 0n && b.decimals !== null ? inputText(amountB, b.decimals) : '') : textB;
  const balanceA = a.held;
  const balanceB = b.held;
  const dep = amountA !== null && amountB !== null && amountA > 0n && amountB > 0n
    ? deposit(amountA, amountB, pool.reserveA, pool.reserveB, totalShares) : null;
  const minShares = dep !== null && bps !== null ? withSlippage(dep.shares, bps) : null;
  // a KS-20 side carries the deposit: its token's transfer_and_call sends it and makes the deposit, the other side attached when
  // it is an asset; a token side it does not carry is pulled at the amount the pool keeps (never more than the amount given)
  const carrier: PoolSide | null = a.kind === 'Token' && a.ks20 ? a : b.kind === 'Token' && b.ks20 ? b : null;
  const pulledA = a.kind === 'Token' && carrier !== a;
  const pulledB = b.kind === 'Token' && carrier !== b;
  const approvalA = pulledA ? approvalNeeded(a, amountA) : null;
  const approvalB = pulledB ? approvalNeeded(b, amountB) : null;

  // what stands in the way of the deposit besides approvals, which are offered only once nothing else does
  const blocked = ((): Reason => {
    if (a.decimals === null || b.decimals === null) return { tone: 'hint', text: t.core.common.loading };
    if (filled) {
      const typedText = anchor === 'a' ? textA : textB;
      const typed = anchor === 'a' ? typedA : typedB;
      const other = anchor === 'a' ? amountB : amountA;
      if (!typedText.trim()) return { tone: 'hint', text: m.enterOne };
      if (typed === null) return 'shown';
      if (typed === 0n) return { tone: 'error', text: m.zero };
      if (other === null) return { tone: 'error', text: m.tooLarge };
      if (other === 0n) return { tone: 'error', text: m.depositTooSmall };
    } else {
      if (!textA.trim() || !textB.trim()) return { tone: 'hint', text: m.enterBoth };
      if (typedA === null || typedB === null) return 'shown';
      if (typedA === 0n || typedB === 0n) return { tone: 'error', text: m.zero };
    }
    if ((balanceA !== null && amountA !== null && amountA > balanceA) || (balanceB !== null && amountB !== null && amountB > balanceB)) return 'shown';
    if (dep === null) return { tone: 'error', text: m.tooLarge };
    if (dep.shares === 0n) return { tone: 'error', text: m.depositTooSmall };
    if (bps === null) return 'shown';
    return null;
  })();
  const allowanceLoads = !!wallet && ((pulledA && a.allowance === null) || (pulledB && b.allowance === null));
  const approveA = blocked === null && !allowanceLoads ? approvalA : null;
  const approveB = blocked === null && !allowanceLoads ? approvalB : null;
  const reason: Reason = blocked ?? (allowanceLoads ? { tone: 'hint', text: t.core.common.loading }
    : approveA !== null ? { tone: 'hint', text: m.approveFirst(a.name) }
      : approveB !== null ? { tone: 'hint', text: m.approveFirst(b.name) } : null);

  const clear = () => {
    setDone(null);
    setLocal(null);
    sender.clear();
  };
  const send = () => {
    if (!wallet || amountA === null || amountB === null || dep === null || minShares === null) return;
    const other = carrier === a ? b : a;
    const own = carrier === a ? amountA : amountB;
    const otherAmount = carrier === a ? amountB : amountA;
    const action = carrier ? buildSend(carrier, contract, own, { variant: 'AddLiquidity', values: [otherAmount, minShares] }, attached([[other, otherAmount]]))
      : buildCall(abi, contract, 'add_liquidity', [amountA, amountB, minShares], attached([[a, amountA], [b, amountB]]));
    if (typeof action === 'string') {
      setLocal(action);
      return;
    }
    const putA = amountText(a, amountA);
    const putB = amountText(b, amountB);
    void sender.send({
      title: m.addTitle(putA, putB),
      action,
      details: [
        ...(carrier ? [{ label: m.sentWith, value: m.viaToken(carrier.name) }] : []),
        { label: a.name, value: putA },
        { label: b.name, value: putB },
        { label: m.minShares, value: formatUnits(minShares, 0) },
      ],
    }).then((activity) => {
      if (!activity) return;
      setTextA('');
      setTextB('');
      const got = carrier ? answerIn(activity) : outputOf(abi, 'add_liquidity', activity);
      setDone(typeof got === 'bigint' ? m.addDone(formatUnits(got, 0)) : null);
    });
  };

  const price = !filled && amountA !== null && amountB !== null && amountA > 0n && a.decimals !== null && b.decimals !== null
    ? unitPrice(amountA, amountB, a.decimals, b.decimals) : null;
  // the excess of an asset side and of the side its token sends comes back; a pulled token side is pulled only at the amount used
  const back = dep !== null && amountA !== null && amountB !== null
    ? [!pulledA && amountA > dep.usedA ? m.refund(amountText(a, amountA - dep.usedA)) : '',
      !pulledB && amountB > dep.usedB ? m.refund(amountText(b, amountB - dep.usedB)) : '']
      .filter(Boolean).join(' ') : '';
  const pulledOther = carrier === a ? pulledB : pulledA;
  const howHint = carrier ? (pulledOther ? m.oneStepPulledHint(carrier.name, (carrier === a ? b : a).name) : m.oneStepAddHint(carrier.name))
    : a.kind === 'Token' || b.kind === 'Token' ? m.tokenRefundHint : m.refundHint;
  return <div className="form-grid" hidden={hidden}>
    {!filled && <Banner tone="info">{m.firstDeposit}</Banner>}
    <SideInput label={m.putIn(a.name)} side={a} value={shownA} onChange={(v) => { setAnchor('a'); setTextA(v); clear(); }} amount={amountA} pulled={pulledA} />
    <SideInput label={m.putIn(b.name)} side={b} value={shownB} onChange={(v) => { setAnchor('b'); setTextB(v); clear(); }} amount={amountB} pulled={pulledB} />
    {filled ? <p className="field-hint">{m.ratioHint} {howHint}</p> : carrier && <p className="field-hint">{howHint}</p>}
    {dep !== null && dep.shares > 0n && <div className="outcome">
      <span className="outcome-label">{m.sharesYouGet}</span>
      <span className="amm-figure">{m.sharesOfPool(formatUnits(dep.shares, 0), percentText(dep.shares, totalShares + dep.shares))}</span>
      {minShares !== null && <span className="field-hint">{m.atLeastShares(formatUnits(minShares, 0))}</span>}
      {price !== null && <span className="field-hint">{m.setsPrice(`1 ${a.name} = ${price} ${b.name}`)}</span>}
      {filled && <span className="field-hint">{m.pooled(amountText(a, dep.usedA), amountText(b, dep.usedB))}{back && ` ${back}`}</span>}
    </div>}
    <SlippageField value={slippage} onChange={onSlippage} />
    <ReasonLine reason={reason} />
    <div className="method-actions">
      {wallet && approveA !== null && <ApproveButton approver={approver} pool={contract} side={a} amount={approveA} />}
      {wallet && approveB !== null && <ApproveButton approver={approver} pool={contract} side={b} amount={approveB} />}
      <Button variant="primary" disabled={!wallet || reason !== null || sender.busy} busy={sender.busy} onClick={send}>{m.addButton}</Button>
    </div>
    {approver.error && <Banner tone="danger">{approver.error}</Banner>}
    {done && <Banner tone="ok">{done}</Banner>}
    {(local ?? sender.error) && <Banner tone="danger">{local ?? sender.error}</Banner>}
  </div>;
}

function RemoveForm({ contract, abi, pool, slippage, onSlippage, hidden, mine }: FormProps & { mine: bigint | null }) {
  const t = useT();
  const m = t.amm;
  const wallet = useWallet();
  const sender = useSend();
  const [text, setText] = useState('');
  const [done, setDone] = useState<string | null>(null);
  const [local, setLocal] = useState<string | null>(null);
  const { a, b, totalShares } = pool;
  const bps = slippageBps(slippage);
  const shares = amountOf(text, 0);
  const over = shares !== null && mine !== null && shares > mine;
  const out = shares !== null && shares > 0n && !over ? withdraw(shares, pool.reserveA, pool.reserveB, totalShares) : null;
  const minA = out !== null && bps !== null ? withSlippage(out.a, bps) : null;
  const minB = out !== null && bps !== null ? withSlippage(out.b, bps) : null;

  const reason = ((): Reason => {
    if (a.decimals === null || b.decimals === null) return { tone: 'hint', text: t.core.common.loading };
    if (!text.trim()) return { tone: 'hint', text: m.enterShares };
    if (shares === null || over) return 'shown';
    if (shares === 0n) return { tone: 'error', text: m.zero };
    if (out === null) return { tone: 'error', text: m.tooLarge };
    if (out.a === 0n || out.b === 0n) return { tone: 'error', text: m.removeTooSmall };
    if (bps === null) return 'shown';
    return null;
  })();

  const edit = (v: string) => {
    setText(v);
    setDone(null);
    setLocal(null);
    sender.clear();
  };
  const send = () => {
    if (!wallet || shares === null || out === null || minA === null || minB === null || bps === null) return;
    const action = buildCall(abi, contract, 'remove_liquidity', [shares, minA, minB]);
    if (typeof action === 'string') {
      setLocal(action);
      return;
    }
    void sender.send({
      title: m.removeTitle(amountText(a, out.a), amountText(b, out.b)),
      action,
      details: [
        { label: m.sharesLabel, value: formatUnits(shares, 0) },
        { label: m.minReceived, value: m.both(amountText(a, minA), amountText(b, minB)) },
        { label: m.slippage, value: `${bpsText(bps)}%` },
      ],
    }).then((activity) => {
      if (!activity) return;
      setText('');
      const got = outputOf(abi, 'remove_liquidity', activity);
      setDone(Array.isArray(got) && typeof got[0] === 'bigint' && typeof got[1] === 'bigint'
        ? m.removeDone(amountText(a, got[0]), amountText(b, got[1])) : null);
    });
  };

  return <div className="form-grid" hidden={hidden}>
    <p className="field-hint">{m.removeHint}</p>
    {!wallet ? null : mine === null ? <p className="muted">{t.core.common.loading}</p> : mine === 0n ? <p>{m.noSharesToRemove}</p> : <>
      <div className="amm-field">
        <UnitsField label={m.sharesToRemove} unit={m.sharesUnit} value={text} onChange={edit} decimals={0}
          hint={m.yourShares(formatUnits(mine, 0), percentText(mine, totalShares))} />
        {over && <span className="field-error">{m.moreThanShares}</span>}
      </div>
      <div className="method-actions" role="group" aria-label={m.partOfYours}>
        {[25n, 50n, 75n, 100n].map((p) => <Button key={p.toString()} size="sm" onClick={() => edit(((mine * p) / 100n).toString())}>{p.toString()}%</Button>)}
      </div>
      {out !== null && out.a > 0n && out.b > 0n && <div className="outcome">
        <span className="outcome-label">{m.youGetBack}</span>
        <span className="amm-figure">{m.both(amountText(a, out.a), amountText(b, out.b))}</span>
        {minA !== null && minB !== null && <span className="field-hint">{m.atLeastBoth(amountText(a, minA), amountText(b, minB))}</span>}
        {(a.baseUnits || b.baseUnits) && <span className="field-hint">{m.baseUnits}</span>}
      </div>}
      <SlippageField value={slippage} onChange={onSlippage} />
      <ReasonLine reason={reason} />
      <div className="method-actions">
        <Button variant="primary" disabled={reason !== null || sender.busy} busy={sender.busy} onClick={send}>{m.removeButton}</Button>
      </div>
      {done && <Banner tone="ok">{done}</Banner>}
      {(local ?? sender.error) && <Banner tone="danger">{local ?? sender.error}</Banner>}
    </>}
  </div>;
}
