/** The asset its contract issues: its supply against the cap, your balance, sending it, and the owner's mint, burns and the hand-over. */
import { useState } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import { formatUnits, parseUnits } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { requestDeploy } from '../../app/deploys';
import { navigate } from '../../app/route';
import { useT } from '../../i18n';
import { ContractEvents } from '../../ui/Events';
import { Banner, Button, Id, Meter, Rows, Section, Stat } from '../../ui/ui';
import { AccountField, AccountText, UnitsField, accountOf, sameAccount } from '../../ui/values';
import { HowItWorks } from '../Prose';
import { callAction, useSend, type AppProps } from '../kit';

interface Info {
  asset: Uint8Array;
  name: string;
  symbol: string;
  decimals: bigint;
  owner: Uint8Array | null;
  cap: bigint | null;
  supply: bigint;
  minted: bigint;
  burned: bigint;
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

export default function App({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.token;
  const conn = useConnection();
  const wallet = useWallet();
  const info = useView<Info>(contract, abi, 'info');
  const me = wallet ? fromHex(wallet.account.account, 32) : null;
  const balance = useView<bigint>(contract, abi, 'balance_of', me ? [me] : [], { enabled: !!me });
  const d = info.data;
  if (!d) {
    return <Section title={m.overview}>{info.error ? <Banner tone="danger">{info.error}</Banner> : <p className="muted">{t.core.common.loading}</p>}</Section>;
  }
  const decimals = Number(d.decimals);
  const fmt = (v: bigint) => formatUnits(v, decimals);
  const isOwner = sameAccount(d.owner, me);
  const pct = d.cap ? Number((d.supply * 10_000n) / d.cap) / 100 : null;
  return <div className="app-grid">
    <div className="app-col">
      <Section title={<>{d.name} <span className="muted mono">{m.overview}</span></>}>
        <div className="stats">
          <Stat label={m.supply} value={fmt(d.supply)} sub={d.cap ? m.ofCap(pct!.toFixed(2)) : m.noCap} />
          <Stat label={m.yourBalance} value={me ? (balance.data !== null ? fmt(balance.data) : '…') : '—'} />
        </div>
        {d.cap !== null && <Meter value={pct! / 100} label={m.cap} />}
        <Rows rows={[
          { label: m.asset, value: <Id value={toHex(d.asset)} /> },
          { label: m.symbol, value: <span className="mono">{d.symbol}</span> },
          { label: m.decimals, value: <span className="mono">{decimals}</span> },
          { label: m.cap, value: d.cap !== null ? <span className="mono">{fmt(d.cap)}</span> : <span className="muted">{m.noCap}</span> },
          { label: m.minted, value: <span className="mono">{fmt(d.minted)}</span> },
          { label: m.burned, value: <span className="mono">{fmt(d.burned)}</span> },
          {
            label: m.owner,
            value: d.owner ? <span className="id"><AccountText value={d.owner} network={conn?.network} />{isOwner && <span className="pill pill--ok">{m.youOwn}</span>}</span>
              : <span className="muted">{m.renounced}</span>,
          },
        ]} />
        {me && d.owner && !isOwner && <p className="field-hint">{m.notOwner}</p>}
      </Section>
      <SendPanel info={d} />
      <BurnPanel contract={contract} abi={abi} info={d} balance={balance.data} />
      {isOwner && <MintPanel contract={contract} abi={abi} info={d} />}
      {isOwner && <OwnerPanel contract={contract} abi={abi} />}
    </div>
    <div className="app-col">
      <Section title={m.useIt}>
        <p className="field-hint">{m.useHint}</p>
        <div className="method-actions">
          <Button size="sm" onClick={() => requestDeploy('amm', { side_b: { variant: 'Asset', values: [d.asset] } })}>{m.useAmm}</Button>
          <Button size="sm" onClick={() => navigate({ example: 'vesting', tab: 'app', contract: null })}>{m.useVesting}</Button>
          <Button size="sm" onClick={() => requestDeploy('lending', { collateral_asset: d.asset })}>{m.useLending}</Button>
        </div>
      </Section>
      <ContractEvents contract={contract} abi={abi} title={m.recent} />
      <HowItWorks title={m.howTitle} items={m.how} />
    </div>
  </div>;
}

function SendPanel({ info }: { info: Info }) {
  const t = useT();
  const m = t.token;
  const conn = useConnection();
  const wallet = useWallet();
  const [to, setTo] = useState('');
  const [amount, setAmount] = useState('');
  const sender = useSend();
  const decimals = Number(info.decimals);
  const value = amountIn(amount, decimals);
  const target = accountOf(to, conn?.network).account;
  const send = () => {
    if (!value || !target) return;
    void sender.send({
      title: m.sendTitle(formatUnits(value, decimals), info.symbol),
      action: { kind: 'AssetTransfer', asset: info.asset, to: target, amount: value },
    }).then((a) => { if (a) setAmount(''); });
  };
  return <Section title={m.send}>
    <p className="field-hint">{m.sendHint}</p>
    <div className="form-grid">
      <AccountField label={m.to} value={to} onChange={setTo} />
      <UnitsField label={m.amount} unit={info.symbol} value={amount} onChange={setAmount} decimals={decimals} />
      <div className="method-actions">
        <Button variant="primary" disabled={!wallet || !value || !target || sender.busy} busy={sender.busy} onClick={send}>{m.send}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function MintPanel({ contract, abi, info }: AppProps & { info: Info }) {
  const m = useT().token;
  const conn = useConnection();
  const wallet = useWallet();
  const [to, setTo] = useState(wallet?.account.address ?? '');
  const [amount, setAmount] = useState('');
  const sender = useSend();
  const decimals = Number(info.decimals);
  const value = amountIn(amount, decimals);
  const target = accountOf(to, conn?.network).account;
  const room = info.cap !== null ? info.cap - info.supply : null;
  return <Section title={m.mint}>
    <p className="field-hint">{m.mintHint}</p>
    <div className="form-grid">
      <AccountField label={m.to} value={to} onChange={setTo} />
      <UnitsField label={m.amount} unit={info.symbol} value={amount} onChange={setAmount} decimals={decimals} max={room} />
      <div className="method-actions">
        <Button variant="primary" disabled={!value || !target || sender.busy} busy={sender.busy}
          onClick={() => value && target && void sender.send({
            title: m.mintTitle(formatUnits(value, decimals), info.symbol),
            action: callAction(abi, contract, 'mint', [target, value]),
            details: [{ label: m.to, value: to.trim() }, { label: m.amount, value: `${formatUnits(value, decimals)} ${info.symbol}` }],
          }).then((a) => { if (a) setAmount(''); })}>{m.mint}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function BurnPanel({ contract, abi, info, balance }: AppProps & { info: Info; balance: bigint | null }) {
  const m = useT().token;
  const wallet = useWallet();
  const [amount, setAmount] = useState('');
  const sender = useSend();
  const decimals = Number(info.decimals);
  const value = amountIn(amount, decimals);
  if (!wallet || !balance) return null;
  return <Section title={m.burn}>
    <p className="field-hint">{m.burnHint}</p>
    <div className="form-grid">
      <UnitsField label={m.amount} unit={info.symbol} value={amount} onChange={setAmount} decimals={decimals} max={balance} />
      <div className="method-actions">
        <Button variant="danger" disabled={!value || sender.busy} busy={sender.busy}
          onClick={() => value && void sender.send({
            title: m.burnTitle(formatUnits(value, decimals), info.symbol),
            action: callAction(abi, contract, 'burn', [], [{ asset: info.asset, amount: value }]),
          }).then((a) => { if (a) setAmount(''); })}>{m.burn}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function OwnerPanel({ contract, abi }: AppProps) {
  const m = useT().token;
  const conn = useConnection();
  const [to, setTo] = useState('');
  const sender = useSend();
  const target = accountOf(to, conn?.network).account;
  return <Section title={m.ownership}>
    <div className="form-grid">
      <AccountField label={m.newOwner} value={to} onChange={setTo} />
      <div className="method-actions">
        <Button disabled={!target || sender.busy} busy={sender.busy}
          onClick={() => target && void sender.send({
            title: m.handOverTitle, action: callAction(abi, contract, 'set_owner', [target]), details: [{ label: m.newOwner, value: to.trim() }],
          })}>{m.handOver}</Button>
        <Button variant="danger" disabled={sender.busy}
          onClick={() => window.confirm(m.renounceConfirm) && void sender.send({ title: m.renounceTitle, action: callAction(abi, contract, 'set_owner', [null]) })}>
          {m.renounce}
        </Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}
