/** What the wallet holds (KSA, the assets seen in its deposits and proposals, the ones watched) and depositing into it. */
import { useId, useState } from 'react';
import { fromHex } from '@ksana/sdk';
import { useAccount } from '../../chain/account';
import { KSA_HEX, formatUnits, useAssetMeta } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { useWallet } from '../../chain/wallet';
import { useT } from '../../i18n';
import { Banner, Button, Id, Section } from '../../ui/ui';
import { Amount, AmountField, AssetName, amountOf } from '../../ui/values';
import { callAction, type AppProps } from '../kit';
import { assetHexOf, idHexOf } from './actions';
import { AssetSelect, useAct } from './parts';
import type { ReadState } from './reads';

export function BalancesPanel({ assets, balances, watched, found, onWatch, onUnwatch }: {
  assets: readonly string[];
  balances: ReadState<ReadonlyMap<string, bigint>>;
  watched: readonly string[];
  /** The assets listed without being watched */
  found: ReadonlySet<string>;
  onWatch: (asset: string) => void;
  onUnwatch: (asset: string) => void;
}) {
  const t = useT();
  const m = t.multisig;
  const id = useId();
  const [text, setText] = useState('');
  const asset = idHexOf(text);
  const watch = () => {
    if (!asset) return;
    onWatch(asset);
    setText('');
  };
  return <Section title={m.balances}>
    {balances.error && <Banner tone="danger">{balances.error}</Banner>}
    <ul className="list">
      {assets.map((a) => {
        const held = balances.data?.get(a);
        return <li key={a} className="list-item">
          <span className="multisig-asset"><AssetName asset={a} />{a !== KSA_HEX && <Id value={a} />}</span>
          <span className="multisig-asset">
            {held !== undefined ? <Amount value={held} asset={a} /> : <span className="muted">…</span>}
            {watched.includes(a) && !found.has(a) && <Button size="sm" variant="ghost" onClick={() => onUnwatch(a)}>{m.unwatch}</Button>}
          </span>
        </li>;
      })}
    </ul>
    <p className="field-hint multisig-gap">{m.balancesHint}</p>
    <div className="vfield multisig-gap">
      <label className="field-label" htmlFor={id}>{m.watchLabel}</label>
      <div className="input-row">
        <input id={id} className="input mono" spellCheck={false} autoCapitalize="off" autoCorrect="off" autoComplete="off" value={text}
          placeholder={t.core.form.idHint} onChange={(e) => setText(e.target.value)} onKeyDown={(e) => { if (e.key === 'Enter') watch(); }} />
        <Button size="sm" disabled={!asset} onClick={watch}>{m.watch}</Button>
      </div>
      {text.trim() !== '' && !asset && <span className="field-error">{m.badAsset}</span>}
    </div>
  </Section>;
}

export function DepositPanel({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.multisig;
  const conn = useConnection();
  const wallet = useWallet();
  const account = useAccount();
  const act = useAct();
  const [asset, setAsset] = useState(KSA_HEX);
  const [amount, setAmount] = useState('');
  // the assets the connected account holds, KSA first
  const own = [KSA_HEX, ...(account ? [...account.balances].filter(([a, v]) => a !== KSA_HEX && v > 0n).map(([a]) => a) : [])];
  const hex = assetHexOf(asset);
  const meta = useAssetMeta(conn, hex);
  const value = meta ? amountOf(amount, meta.decimals) : null;
  const held = hex && account ? account.balances.get(hex) ?? 0n : null;
  const over = value !== null && held !== null && value > held;
  const ok = value !== null && value > 0n && !over;
  const send = () => {
    if (!ok || !hex || !meta || value === null) return;
    void act.send('deposit', m.depositTitle(`${formatUnits(value, meta.decimals)} ${meta.name}`),
      () => callAction(abi, contract, 'deposit', [], [{ asset: fromHex(hex, 32), amount: value }])).then((done) => { if (done) setAmount(''); });
  };
  return <Section title={m.deposit}>
    <p className="field-hint">{m.depositHint}</p>
    <div className="form-grid multisig-gap">
      <AssetSelect label={m.asset} options={own} value={asset} onChange={setAsset} />
      {hex && <AmountField label={m.amount} asset={hex} value={amount} onChange={setAmount} />}
      {value === 0n && <p className="field-error">{m.problems.zeroAmount}</p>}
      {over && <p className="field-error">{m.overBalance}</p>}
      <div className="method-actions">
        <Button variant="primary" disabled={!wallet || !ok || act.busy} busy={act.busy} onClick={send}>{m.deposit}</Button>
      </div>
      {act.error && <Banner tone="danger">{act.error}</Banner>}
    </div>
  </Section>;
}
