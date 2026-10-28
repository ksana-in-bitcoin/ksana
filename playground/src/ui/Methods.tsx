/**
 * Forms for every method of a contract, generated from its ABI block: views are read at once, calls can be tried read-only
 * (nothing signed or sent) and then sent through the review. Results and events are decoded with the same ABI.
 */
import { useState } from 'react';
import { toHex, type Funds, type Receipt } from '@ksana/sdk';
import { decodeEvent, decodeReturn, encodeArgs } from '../abi/codec';
import { InputError, emptyDraft, formatValue, valueOf, type Draft } from '../abi/text';
import { typeName, type Abi, type MethodDef, type Value } from '../abi/types';
import { assetMeta } from '../chain/assets';
import { useConnection } from '../chain/connection';
import { writeKnowingContracts } from '../chain/contracts';
import { failureText, readMethod } from '../chain/views';
import { useWallet } from '../chain/wallet';
import { callAction, useSend } from '../examples/kit';
import { useT } from '../i18n';
import { errorText } from './errors';
import { Banner, Button, Empty, Section } from './ui';
import { FundsEditor, ValueEditor, ValueView, fundsOf, type FundsDraft } from './values';

export function MethodsPanel({ contract, abi }: { contract: string | null; abi: Abi | null }) {
  const m = useT().core.methods;
  if (!abi) return <Empty>{m.noAbi}</Empty>;
  const views = abi.methods.filter((x) => x.kind === 'view');
  const calls = abi.methods.filter((x) => x.kind === 'call');
  return <div className="methods">
    {views.length > 0 && <Section title={m.views}>{views.map((x) => <MethodCard key={x.name} abi={abi} method={x} contract={contract} />)}</Section>}
    {calls.length > 0 && <Section title={m.calls}>{calls.map((x) => <MethodCard key={x.name} abi={abi} method={x} contract={contract} />)}</Section>}
  </div>;
}

type Outcome =
  | { kind: 'value'; value: Value }
  | { kind: 'receipt'; receipt: Receipt }
  | { kind: 'error'; text: string };

function signature(method: MethodDef): string {
  const params = method.params.map((p) => `${p.name}: ${typeName(p.type)}`).join(', ');
  return `${method.name}(${params})${method.returns ? ` → ${typeName(method.returns)}` : ''}`;
}

function MethodCard({ abi, method, contract }: { abi: Abi; method: MethodDef; contract: string | null }) {
  const t = useT();
  const m = t.core.methods;
  const conn = useConnection();
  const wallet = useWallet();
  const [open, setOpen] = useState(method.kind === 'view' && method.params.length === 0);
  const [drafts, setDrafts] = useState<Draft[]>(() => method.params.map((p) => emptyDraft(abi, p.type)));
  const [funds, setFunds] = useState<FundsDraft[]>([]);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [running, setRunning] = useState(false);
  const sender = useSend();

  const collect = async (): Promise<{ args: Value[]; funds: Funds[] }> => {
    const args = method.params.map((p, i) => valueOf(abi, p.type, drafts[i] ?? emptyDraft(abi, p.type), { network: conn?.network }, p.name));
    const attached = method.payable && conn ? await fundsOf(funds, async (asset) => (await assetMeta(conn, asset)).decimals) : [];
    return { args, funds: attached };
  };

  const describe = (e: unknown) => (e instanceof InputError ? `${e.path}: ${e.message}` : errorText(e, t));

  const read = async () => {
    if (!conn || !contract) return;
    setRunning(true);
    try {
      const { args, funds: attached } = await collect();
      const value = await readMethod(conn, contract, abi, method.name, args, { caller: wallet?.account.account ?? null, funds: attached });
      setOutcome({ kind: 'value', value });
    } catch (e) {
      setOutcome({ kind: 'error', text: describe(e) });
    } finally {
      setRunning(false);
    }
  };

  const simulate = async () => {
    if (!conn || !contract) return;
    setRunning(true);
    try {
      const { args, funds: attached } = await collect();
      const receipt = await conn.node.call({
        contract, method: method.name, args: encodeArgs(abi, method.params.map((p) => p.type), args),
        ...(attached.length ? { funds: attached } : {}),
        ...(wallet ? { caller: wallet.account.account } : {}),
      });
      setOutcome({ kind: 'receipt', receipt });
    } catch (e) {
      setOutcome({ kind: 'error', text: describe(e) });
    } finally {
      setRunning(false);
    }
  };

  const send = async () => {
    if (!conn || !contract) return;
    let collected: { args: Value[]; funds: Funds[] };
    try {
      collected = await collect();
    } catch (e) {
      setOutcome({ kind: 'error', text: describe(e) });
      return;
    }
    setOutcome(null);
    const details = await writeKnowingContracts(conn, (isContract) => method.params.map((p, i) => ({
      label: p.name, value: formatValue(abi, p.type, collected.args[i] ?? null, { network: conn.network, contract: isContract }),
    })));
    const activity = await sender.send({
      title: `${method.name}()`,
      action: callAction(abi, contract, method.name, collected.args, collected.funds),
      details,
    });
    const receipt = activity?.status?.receipt;
    if (receipt) setOutcome({ kind: 'receipt', receipt });
  };

  return <details className="method" open={open} onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}>
    <summary>
      <span className="method-name mono">{signature(method)}</span>
      {method.payable && <span className="pill pill--info">{m.payable}</span>}
    </summary>
    {method.doc && <p className="method-doc">{method.doc}</p>}
    <div className="method-form">
      {method.params.map((p, i) => <ValueEditor key={p.name} abi={abi} type={p.type} label={p.name} draft={drafts[i] ?? emptyDraft(abi, p.type)}
        onChange={(d) => setDrafts(drafts.map((x, j) => (j === i ? d : x)))} />)}
      {method.payable && <FundsEditor rows={funds} onChange={setFunds} />}
      <div className="method-actions">
        {method.kind === 'view'
          ? <Button variant="primary" size="sm" disabled={!conn || !contract || running} busy={running} onClick={() => void read()}>{m.read}</Button>
          : <>
            <Button size="sm" disabled={!conn || !contract || running} busy={running} onClick={() => void simulate()}>{m.simulate}</Button>
            <Button variant="primary" size="sm" disabled={!conn || !contract || !wallet || sender.busy} busy={sender.busy} onClick={() => void send()}>{m.send}</Button>
          </>}
        {!wallet && method.kind === 'call' && <span className="field-hint">{t.core.errors.connectFirst}</span>}
      </div>
      {method.kind === 'call' && <p className="field-hint">{m.simNote}</p>}
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
      {outcome && <OutcomeView abi={abi} method={method} outcome={outcome} />}
    </div>
  </details>;
}

function OutcomeView({ abi, method, outcome }: { abi: Abi; method: MethodDef; outcome: Outcome }) {
  const m = useT().core.methods;
  if (outcome.kind === 'error') return <Banner tone="danger">{outcome.text}</Banner>;
  if (outcome.kind === 'value') {
    return <div className="outcome">
      <div className="outcome-label">{m.result}</div>
      {method.returns ? <ValueView abi={abi} type={method.returns} value={outcome.value} /> : <span className="muted">{m.noResult}</span>}
    </div>;
  }
  const r = outcome.receipt;
  if (!r.success) {
    return <div className="outcome outcome--failed">
      <Banner tone="warn">{m.failed(failureText(r.error))}</Banner>
      <div className="muted">{m.gasUsed(r.gas_used.toLocaleString())}</div>
    </div>;
  }
  let decoded: { ok: true; value: Value } | { ok: false } = { ok: false };
  try {
    decoded = { ok: true, value: decodeReturn(abi, method.returns, r.output) };
  } catch {
    decoded = { ok: false };
  }
  return <div className="outcome">
    <div className="outcome-label">{m.result} <span className="muted">· {m.gasUsed(r.gas_used.toLocaleString())}</span></div>
    {decoded.ok
      ? (method.returns ? <ValueView abi={abi} type={method.returns} value={decoded.value} /> : <span className="muted">{m.noResult}</span>)
      : <span className="mono break">{m.raw}: 0x{toHex(r.output)}</span>}
    <div className="outcome-label">{m.events}</div>
    {r.events.length === 0 ? <span className="muted">{m.noEvents}</span> : <ul className="events">
      {r.events.map((ev, i) => <EventRow key={i} abi={abi} data={ev.data} />)}
    </ul>}
  </div>;
}

export function EventRow({ abi, data }: { abi: Abi; data: Uint8Array }) {
  const decoded = decodeEvent(abi, data);
  if (!decoded) return <li className="event"><span className="mono break">0x{toHex(data)}</span></li>;
  const def = abi.events.find((e) => e.name === decoded.name);
  return <li className="event">
    <span className="event-name mono">{decoded.name}</span>
    <dl className="vrows">{decoded.fields.map((f, i) => {
      const type = def?.fields[i]?.type;
      return <div key={f.name}><dt>{f.name}</dt><dd>{type ? <ValueView abi={abi} type={type} value={f.value} /> : '?'}</dd></div>;
    })}</dl>
  </li>;
}
