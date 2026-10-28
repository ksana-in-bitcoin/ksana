/** The counter: the count, two ways to increase it, and the latest changes from the contract's events. */
import { useState } from 'react';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useT } from '../../i18n';
import { ContractEvents } from '../../ui/Events';
import { Banner, Button, Field, Section } from '../../ui/ui';
import { AccountText } from '../../ui/values';
import { useConnection } from '../../chain/connection';
import { HowItWorks } from '../Prose';
import { callAction, useSend, type AppProps } from '../kit';

export default function App({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.counter;
  const wallet = useWallet();
  const conn = useConnection();
  const count = useView<bigint>(contract, abi, 'get');
  const one = useSend();
  const many = useSend();
  const [amount, setAmount] = useState('10');
  const n = /^\d{1,4}$/.test(amount.trim()) ? BigInt(amount.trim()) : null;
  const valid = n !== null && n >= 1n && n <= 1000n;
  return <div className="app-grid">
    <div className="app-col">
      <Section title={m.count}>
        <div className="big-number" aria-live="polite">{count.data !== null ? count.data.toLocaleString() : '…'}</div>
        <p className="field-hint">{m.hint}</p>
        {count.error && <Banner tone="danger">{count.error}</Banner>}
        <div className="form-grid">
          <div className="method-actions">
            <Button variant="primary" disabled={!wallet || one.busy} busy={one.busy}
              onClick={() => void one.send({ title: m.incrementTitle, action: callAction(abi, contract, 'increment', []) })}>{m.increment}</Button>
          </div>
          <div className="input-row">
            <Field label={m.amount} htmlFor="counter-add" error={amount.trim() && !valid ? m.badAmount : null}>
              <input id="counter-add" className="input mono" inputMode="numeric" value={amount} onChange={(e) => setAmount(e.target.value)} />
            </Field>
            <Button disabled={!wallet || !valid || many.busy} busy={many.busy} className="align-end"
              onClick={() => n !== null && void many.send({
                title: m.addTitle(n.toString()), action: callAction(abi, contract, 'add', [n]), details: [{ label: 'amount', value: n.toString() }],
              })}>{m.add}</Button>
          </div>
          {!wallet && <p className="field-hint">{t.core.errors.connectFirst}</p>}
          {(one.error || many.error) && <Banner tone="danger">{one.error ?? many.error}</Banner>}
        </div>
      </Section>
      <ContractEvents contract={contract} abi={abi} title={m.recent} only={['changed']}
        render={(e) => <p>{e.values.by instanceof Uint8Array && <AccountText value={e.values.by} network={conn?.network} />} {m.changed(String(e.values.value))}</p>} />
    </div>
    <HowItWorks title={m.howTitle} items={m.how} />
  </div>;
}
