/**
 * The KS-20 token: its supply against the cap and your balance, sending it, allowances with an expiry and taking with one,
 * burning, and the minter's mint and hand-over.
 */
import { useState } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import type { Value } from '../../abi/types';
import { requestDeploy } from '../../app/deploys';
import { formatUnits, parseUnits } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { useNamed } from '../../chain/ids';
import { dateLocale, formatDateTime, useChainTime } from '../../chain/time';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useLocale, useT, type Messages } from '../../i18n';
import { ContractEvents, type ContractEvent } from '../../ui/Events';
import { ExpiryField, expiryOf, isExpired, useExpiryText, type ExpiryChoice } from '../../ui/expiry';
import { Banner, Button, Id, Meter, Rows, Section, Sentence, Stat } from '../../ui/ui';
import { AccountField, AccountText, UnitsField, accountOf, sameAccount } from '../../ui/values';
import { HowItWorks } from '../Prose';
import { callAction, useSend, type AppProps } from '../kit';
import './ks20.css';

/** An allowance of `u128::MAX` is unlimited and never goes down (KS-20 rule 6) */
const UNLIMITED = (1n << 128n) - 1n;

interface Token {
  name: string;
  symbol: string;
  decimals: number;
  supply: bigint;
  /** `KS-20-mintable`: who mints and up to what; a token without the extension has neither */
  mintable: boolean;
  cap: bigint | null;
  minter: Uint8Array | null;
}

function isZero(account: Uint8Array): boolean {
  return account.every((b) => b === 0);
}

function amountIn(text: string, decimals: number): bigint | null {
  if (!text.trim()) return null;
  try {
    const v = parseUnits(text, decimals);
    return v > 0n ? v : null;
  } catch {
    return null;
  }
}

/** An amount of the token with its symbol */
function amountText(token: Token, value: bigint): string {
  return `${formatUnits(value, token.decimals)} ${token.symbol}`;
}

export default function App({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.ks20;
  const conn = useConnection();
  const wallet = useWallet();
  const me = wallet ? fromHex(wallet.account.account, 32) : null;
  const metadata = useView<[string, string, bigint]>(contract, abi, 'metadata');
  const supply = useView<bigint>(contract, abi, 'total_supply');
  const standards = useView<string[]>(contract, abi, 'standards');
  // the extensions the token declares (docs/STANDARDS.md §2); a token that does not answer declares none
  const declared = standards.data ?? (standards.ready ? [] : null);
  const mintable = declared?.includes('KS-20-mintable') ?? false;
  const cap = useView<bigint | null>(contract, abi, 'cap', [], { enabled: mintable });
  const minter = useView<Uint8Array | null>(contract, abi, 'minter', [], { enabled: mintable });
  const balance = useView<bigint>(contract, abi, 'balance_of', me ? [me] : [], { enabled: !!me });
  const error = metadata.error ?? supply.error ?? cap.error ?? minter.error;
  if (!metadata.data || supply.data === null || declared === null || (mintable && (!cap.ready || !minter.ready))) {
    return <Section title={m.overview}>{error ? <Banner tone="danger">{error}</Banner> : <p className="muted">{t.core.common.loading}</p>}</Section>;
  }
  const [name, symbol, decimals] = metadata.data;
  const token: Token = {
    name, symbol, decimals: Number(decimals), supply: supply.data, mintable, cap: mintable ? cap.data : null, minter: mintable ? minter.data : null,
  };
  const burnable = declared.includes('KS-20-burnable');
  const fmt = (v: bigint) => formatUnits(v, token.decimals);
  const isMinter = sameAccount(token.minter, me);
  const pct = token.cap !== null && token.cap > 0n ? Number((token.supply * 10_000n) / token.cap) / 100 : null;
  const held = me ? balance.data : null;
  return <div className="app-grid">
    <div className="app-col">
      <Section title={<>{name} <span className="muted mono">{symbol}</span></>}>
        {!declared.includes('KS-20') && <Banner tone="warn">{m.notKs20}</Banner>}
        <div className="stats">
          <Stat label={m.supply} value={fmt(token.supply)} sub={pct !== null ? m.ofCap(pct.toFixed(2)) : mintable ? m.noCap : m.fixedSupply} />
          <Stat label={m.yourBalance} value={me ? (held !== null ? fmt(held) : '…') : '—'} />
        </div>
        {pct !== null && <Meter value={pct / 100} label={m.cap} />}
        <Rows rows={[
          { label: m.contract, value: <Id value={contract} /> },
          { label: m.decimals, value: <span className="mono">{token.decimals}</span> },
          mintable && { label: m.cap, value: token.cap !== null ? <span className="mono">{fmt(token.cap)}</span> : <span className="muted">{m.noCap}</span> },
          mintable && {
            label: m.minter,
            value: token.minter ? <span className="id"><AccountText value={token.minter} network={conn?.network} />{isMinter && <span className="pill pill--ok">{m.you}</span>}</span>
              : <span className="muted">{m.noMinter}</span>,
          },
          declared.length > 0 && {
            label: m.standards, value: <span className="id">{declared.map((id) => <span key={id} className="pill pill--quiet mono">{id}</span>)}</span>,
          },
        ]} />
        <p className="field-hint">{m.contractHint}</p>
        {me && token.minter && !isMinter && <p className="field-hint">{m.notMinter}</p>}
      </Section>
      <SendPanel contract={contract} abi={abi} token={token} balance={held} />
      {me && <Allowances contract={contract} abi={abi} token={token} me={me} />}
      {me && <PullPanel contract={contract} abi={abi} token={token} me={me} />}
      {burnable && me && held !== null && held > 0n && <BurnPanel contract={contract} abi={abi} token={token} balance={held} />}
      {isMinter && <MintPanel contract={contract} abi={abi} token={token} />}
      {isMinter && me && <MinterPanel contract={contract} abi={abi} me={me} />}
    </div>
    <div className="app-col">
      <Section title={m.useIt}>
        <p className="field-hint">{m.useHint}</p>
        <div className="method-actions">
          <Button size="sm" onClick={() => requestDeploy('amm', { side_b: { variant: 'Token', values: [fromHex(contract, 32)] } })}>{m.useAmm}</Button>
        </div>
      </Section>
      <ContractEvents contract={contract} abi={abi} title={m.recent} render={(e) => <EventLine e={e} token={token} />} />
      <HowItWorks title={m.howTitle} items={m.how} />
    </div>
  </div>;
}

/** Why the token cannot go to this account, or null: KS-20 refuses the zero account and the token contract itself */
function recipientProblem(account: Uint8Array | null, contract: string, m: Messages['ks20']): string | null {
  if (!account) return null;
  if (isZero(account)) return m.errZero;
  if (toHex(account) === contract) return m.errToken;
  return null;
}

function SendPanel({ contract, abi, token, balance }: AppProps & { token: Token; balance: bigint | null }) {
  const m = useT().ks20;
  const conn = useConnection();
  const wallet = useWallet();
  const [to, setTo] = useState('');
  const [amount, setAmount] = useState('');
  const sender = useSend();
  const value = amountIn(amount, token.decimals);
  const target = accountOf(to, conn?.network).account;
  const problem = recipientProblem(target, contract, m);
  const named = useNamed(target && !problem ? toHex(target) : null);
  const send = () => {
    if (!value || !target) return;
    void sender.send({
      title: m.sendTitle(amountText(token, value)),
      action: callAction(abi, contract, 'transfer', [target, value]),
      details: [{ label: m.to, value: to.trim() }, { label: m.amount, value: amountText(token, value) }],
    }).then((a) => { if (a) setAmount(''); });
  };
  return <Section title={m.send}>
    <p className="field-hint">{m.sendHint}</p>
    <div className="form-grid">
      <AccountField label={m.to} value={to} onChange={setTo} />
      {problem && <p className="field-error">{problem}</p>}
      {named?.kind === 'contract' && <Banner tone="warn">{m.toContract}</Banner>}
      <UnitsField label={m.amount} unit={token.symbol} value={amount} onChange={setAmount} decimals={token.decimals} max={balance} />
      <div className="method-actions">
        <Button variant="primary" disabled={!wallet || !value || !target || !!problem || sender.busy} busy={sender.busy} onClick={send}>{m.send}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

/** An allowance as the page words it: what the spender may take now, as set, or that it may take nothing */
function useAllowanceText(token: Token): (amount: bigint, expiresAt: bigint | null, now: bigint) => string {
  const m = useT().ks20;
  const expiryText = useExpiryText();
  return (amount, expiresAt, now) => {
    if (amount === 0n) return m.noAllowance;
    const text = amount === UNLIMITED ? m.unlimited : amountText(token, amount);
    return isExpired(expiresAt, now) ? m.allowanceExpired(text, expiryText(expiresAt, now)) : m.allowanceIs(text, expiryText(expiresAt, now));
  };
}

function Allowances({ contract, abi, token, me }: AppProps & { token: Token; me: Uint8Array }) {
  const m = useT().ks20;
  const conn = useConnection();
  const time = useChainTime();
  const expiryText = useExpiryText();
  const allowanceText = useAllowanceText(token);
  const [text, setText] = useState('');
  const [amount, setAmount] = useState('');
  const [expiry, setExpiry] = useState<ExpiryChoice>('week');
  const [doing, setDoing] = useState<'approve' | 'revoke' | null>(null);
  const sender = useSend();
  const spender = accountOf(text, conn?.network).account;
  const problem = !spender ? null : isZero(spender) ? m.errZero : sameAccount(spender, me) ? m.errSelfSpender : null;
  const asked = !!spender && !problem;
  // as set, expired or not: an expired allowance lets the spender take nothing, and revoking it clears it
  const info = useView<[bigint, bigint | null]>(contract, abi, 'allowance_info', spender ? [me, spender] : [], { enabled: asked });
  const known = asked && info.data !== null;
  const [current, expiresAt] = info.data ?? [0n, null];
  const value = amountIn(amount, token.decimals);
  const set = (approve: boolean) => {
    if (!spender || (approve && !value)) return;
    const until = approve ? expiryOf(expiry, time.now) : null;
    const args: Value[] = approve ? [spender, value, until] : [spender, 0n, null];
    setDoing(approve ? 'approve' : 'revoke');
    void sender.send({
      title: approve && value ? m.approveTitle(amountText(token, value)) : m.revokeTitle,
      action: callAction(abi, contract, 'approve', args),
      details: [
        { label: m.spender, value: text.trim() },
        ...(approve && value ? [{ label: m.amount, value: amountText(token, value) }, { label: m.expires, value: expiryText(until, time.now) }] : []),
      ],
    }).then((a) => {
      setDoing(null);
      if (a && approve) setAmount('');
    });
  };
  return <Section title={m.allowances}>
    <p className="field-hint">{m.allowancesHint}</p>
    <div className="form-grid">
      <AccountField label={m.spender} value={text} onChange={setText} />
      {problem && <p className="field-error">{problem}</p>}
      {asked && !info.error && <p className="field-hint">{known ? allowanceText(current, expiresAt, time.now) : '…'}</p>}
      {info.error && <Banner tone="danger">{info.error}</Banner>}
      <UnitsField label={m.amount} unit={token.symbol} value={amount} onChange={setAmount} decimals={token.decimals} />
      <ExpiryField value={expiry} onChange={setExpiry} time={time} disabled={sender.busy} />
      <div className="method-actions">
        <Button variant="primary" disabled={!asked || !value || sender.busy} busy={sender.busy && doing === 'approve'} onClick={() => set(true)}>{m.approve}</Button>
        <Button variant="danger" disabled={!known || current === 0n || sender.busy} busy={sender.busy && doing === 'revoke'} onClick={() => set(false)}>{m.revoke}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function PullPanel({ contract, abi, token, me }: AppProps & { token: Token; me: Uint8Array }) {
  const m = useT().ks20;
  const conn = useConnection();
  const wallet = useWallet();
  const [from, setFrom] = useState('');
  const [to, setTo] = useState(wallet?.account.address ?? '');
  const [amount, setAmount] = useState('');
  const sender = useSend();
  const owner = accountOf(from, conn?.network).account;
  const target = accountOf(to, conn?.network).account;
  const ownerProblem = !owner ? null : sameAccount(owner, me) ? m.errPullSelf : isZero(owner) ? m.errZero : null;
  const targetProblem = recipientProblem(target, contract, m);
  const asked = !!owner && !ownerProblem;
  // what the owner lets this account take now (0 once expired), and what it holds: the less of the two can move
  const allowance = useView<bigint>(contract, abi, 'allowance', owner ? [owner, me] : [], { enabled: asked });
  const holds = useView<bigint>(contract, abi, 'balance_of', owner ? [owner] : [], { enabled: asked });
  const room = allowance.data !== null && holds.data !== null ? (allowance.data < holds.data ? allowance.data : holds.data) : null;
  const value = amountIn(amount, token.decimals);
  const take = () => {
    if (!owner || !target || !value) return;
    void sender.send({
      title: m.pullTitle(amountText(token, value)),
      action: callAction(abi, contract, 'transfer_from', [owner, target, value]),
      details: [{ label: m.from, value: from.trim() }, { label: m.to, value: to.trim() }, { label: m.amount, value: amountText(token, value) }],
    }).then((a) => { if (a) setAmount(''); });
  };
  return <Section title={m.pull}>
    <p className="field-hint">{m.pullHint}</p>
    <div className="form-grid">
      <AccountField label={m.from} value={from} onChange={setFrom} />
      {ownerProblem && <p className="field-error">{ownerProblem}</p>}
      {asked && allowance.data !== null && <p className="field-hint">{allowance.data === 0n ? m.cannotTake
        : m.canTake(allowance.data === UNLIMITED ? m.unlimited : amountText(token, allowance.data))}</p>}
      {allowance.error && <Banner tone="danger">{allowance.error}</Banner>}
      <AccountField label={m.to} value={to} onChange={setTo} />
      {targetProblem && <p className="field-error">{targetProblem}</p>}
      <UnitsField label={m.amount} unit={token.symbol} value={amount} onChange={setAmount} decimals={token.decimals} max={room} />
      <div className="method-actions">
        <Button variant="primary" disabled={!asked || !target || !!targetProblem || !value || room === 0n || sender.busy} busy={sender.busy} onClick={take}>{m.pull}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function BurnPanel({ contract, abi, token, balance }: AppProps & { token: Token; balance: bigint }) {
  const m = useT().ks20;
  const [amount, setAmount] = useState('');
  const sender = useSend();
  const value = amountIn(amount, token.decimals);
  return <Section title={m.burn}>
    <p className="field-hint">{m.burnHint}</p>
    <div className="form-grid">
      <UnitsField label={m.amount} unit={token.symbol} value={amount} onChange={setAmount} decimals={token.decimals} max={balance} />
      <div className="method-actions">
        <Button variant="danger" disabled={!value || sender.busy} busy={sender.busy}
          onClick={() => value && void sender.send({
            title: m.burnTitle(amountText(token, value)),
            action: callAction(abi, contract, 'burn', [value]),
          }).then((a) => { if (a) setAmount(''); })}>{m.burn}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function MintPanel({ contract, abi, token }: AppProps & { token: Token }) {
  const m = useT().ks20;
  const conn = useConnection();
  const wallet = useWallet();
  const [to, setTo] = useState(wallet?.account.address ?? '');
  const [amount, setAmount] = useState('');
  const sender = useSend();
  const value = amountIn(amount, token.decimals);
  const target = accountOf(to, conn?.network).account;
  const problem = recipientProblem(target, contract, m);
  const room = token.cap !== null ? (token.cap > token.supply ? token.cap - token.supply : 0n) : null;
  return <Section title={m.mint}>
    <p className="field-hint">{m.mintHint}</p>
    <div className="form-grid">
      <AccountField label={m.to} value={to} onChange={setTo} />
      {problem && <p className="field-error">{problem}</p>}
      <UnitsField label={m.amount} unit={token.symbol} value={amount} onChange={setAmount} decimals={token.decimals} max={room} />
      <div className="method-actions">
        <Button variant="primary" disabled={!value || !target || !!problem || room === 0n || sender.busy} busy={sender.busy}
          onClick={() => value && target && void sender.send({
            title: m.mintTitle(amountText(token, value)),
            action: callAction(abi, contract, 'mint', [target, value]),
            details: [{ label: m.to, value: to.trim() }, { label: m.amount, value: amountText(token, value) }],
          }).then((a) => { if (a) setAmount(''); })}>{m.mint}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function MinterPanel({ contract, abi, me }: AppProps & { me: Uint8Array }) {
  const m = useT().ks20;
  const conn = useConnection();
  const [to, setTo] = useState('');
  const [doing, setDoing] = useState<'hand' | 'end' | null>(null);
  const sender = useSend();
  const target = accountOf(to, conn?.network).account;
  const problem = !target ? null : isZero(target) ? m.errZero : sameAccount(target, me) ? m.errAlreadyMinter : null;
  const run = (what: 'hand' | 'end', minter: Uint8Array | null) => {
    setDoing(what);
    void sender.send({
      title: minter ? m.handOverTitle : m.endTitle,
      action: callAction(abi, contract, 'set_minter', [minter]),
      ...(minter ? { details: [{ label: m.newMinter, value: to.trim() }] } : {}),
    }).then((a) => {
      setDoing(null);
      if (a) setTo('');
    });
  };
  return <Section title={m.minterRole}>
    <p className="field-hint">{m.minterHint}</p>
    <div className="form-grid">
      <AccountField label={m.newMinter} value={to} onChange={setTo} />
      {problem && <p className="field-error">{problem}</p>}
      <div className="method-actions">
        <Button disabled={!target || !!problem || sender.busy} busy={sender.busy && doing === 'hand'} onClick={() => target && run('hand', target)}>{m.handOver}</Button>
        <Button variant="danger" disabled={sender.busy} busy={sender.busy && doing === 'end'}
          onClick={() => window.confirm(m.endConfirm) && run('end', null)}>{m.endMinting}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

/** An event of the token as a sentence: a mint is a `transfer` from the zero account, a burn one to it (KS-20 rule 3) */
function EventLine({ e, token }: { e: ContractEvent; token: Token }) {
  const page = useT().ks20;
  const m = page.events;
  const conn = useConnection();
  const locale = dateLocale(useLocale());
  const v = e.values;
  const who = (x: Value | undefined) => (x instanceof Uint8Array ? <AccountText value={x} network={conn?.network} /> : <span className="muted">?</span>);
  const minted = e.name === 'transfer' && v.from instanceof Uint8Array && isZero(v.from);
  const burned = e.name === 'transfer' && v.to instanceof Uint8Array && isZero(v.to);
  const text = e.name === 'standards' ? m.standards
    : e.name === 'metadata' ? m.metadata
      : minted ? m.mint
        : burned ? m.burn
          : e.name === 'transfer' ? m.transfer
            : e.name === 'approve' ? (v.amount === 0n ? m.approveCleared : typeof v.expires_at === 'bigint' ? m.approveUntil : m.approve)
              : e.name === 'minter' ? (v.minter !== null ? (v.previous === null ? m.minterFirst : m.minterHandover) : v.previous === null ? m.minterNone : m.minterEnded)
                : null;
  if (text === null) return null;
  const amount = typeof v.amount !== 'bigint' ? <span className="muted">?</span>
    : <span className="mono">{e.name === 'approve' && v.amount === UNLIMITED ? page.unlimited : amountText(token, v.amount)}</span>;
  const ids = Array.isArray(v.ids) ? v.ids.filter((id) => typeof id === 'string').join(', ') : '?';
  return <p className="ks20-event"><Sentence text={text} parts={{
    to: who(v.to), from: who(v.from), owner: who(v.owner), spender: who(v.spender), previous: who(v.previous), minter: who(v.minter), amount,
    date: typeof v.expires_at === 'bigint' ? formatDateTime(v.expires_at, locale) : '?', standards: <span className="mono">{ids}</span>,
    name: typeof v.name === 'string' ? v.name : '?', symbol: <span className="mono">{typeof v.symbol === 'string' ? v.symbol : '?'}</span>,
    decimals: typeof v.decimals === 'bigint' ? v.decimals.toString() : '?',
  }} /></p>;
}
