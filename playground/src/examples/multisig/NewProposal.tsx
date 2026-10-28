/** The form an owner proposes with: what the proposal does, then its fields, checked against the contract's rules before it can be sent. */
import { useId, useRef, useState } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import { accountText } from '../../abi/text';
import type { Abi } from '../../abi/types';
import { instancesOf } from '../../app/instances';
import { KSA_HEX } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { useIsContract, useKnownContracts } from '../../chain/contracts';
import type { Detail } from '../../chain/run';
import { useWallet } from '../../chain/wallet';
import { useT } from '../../i18n';
import { Banner, Button, Field, Section } from '../../ui/ui';
import { AccountField, UnitsField, accountOf, amountOf } from '../../ui/values';
import { callAction } from '../kit';
import {
  ACTION_KINDS, MAX_ARGS_BYTES, MAX_FUNDS, MAX_OWNERS, ZERO_ID, actionValue, assetHexOf, checkAction, idHexOf, methodOk, parseHexBytes,
  shortfall, sortedFunds, type ActionKind, type FundsItem, type Problem, type WalletAction, type WalletInfo,
} from './actions';
import { AssetSelect, actionText, amountText, useAct } from './parts';
import { useMetas } from './reads';

interface Row {
  key: number;
  asset: string;
  amount: string;
}

/** The arguments shown in the review: the first 64 bytes */
const ARGS_SHOWN = 64;

export function NewProposal({ contract, abi, info, me, assets, balances }: {
  contract: string; abi: Abi; info: WalletInfo; me: string | null; assets: readonly string[]; balances: ReadonlyMap<string, bigint> | null;
}) {
  const t = useT();
  const m = t.multisig;
  const conn = useConnection();
  const wallet = useWallet();
  const network = conn?.network;
  const act = useAct();
  const ids = { target: useId(), method: useId(), args: useId(), remove: useId(), threshold: useId() };
  const [kind, setKind] = useState<ActionKind>('Transfer');
  const [asset, setAsset] = useState(KSA_HEX);
  const [amount, setAmount] = useState('');
  const [to, setTo] = useState('');
  const [target, setTarget] = useState('');
  const [method, setMethod] = useState('');
  const [args, setArgs] = useState('');
  const [rows, setRows] = useState<Row[]>([]);
  const rowKey = useRef(0);
  const [owner, setOwner] = useState('');
  const [removing, setRemoving] = useState('');
  const [threshold, setThreshold] = useState('');

  const assetHex = assetHexOf(asset);
  const rowAssets = rows.map((r) => assetHexOf(r.asset));
  const metas = useMetas([...new Set([assetHex, ...rowAssets].filter((a): a is string => a !== null))]);
  const isOwner = me !== null && info.owners.includes(me);
  const holds = (a: string | null) => {
    const held = a ? balances?.get(a) : undefined;
    return a && held !== undefined ? m.holds(amountText(m, metas, a, held)) : undefined;
  };

  // Transfer
  const meta = assetHex ? metas.get(assetHex) : undefined;
  const value = meta ? amountOf(amount, meta.decimals) : null;
  const toAccount = accountOf(to, network).account;
  // the review names a typed account by its ID when it is a contract: the node is asked as it is typed
  useIsContract(kind === 'Transfer' ? toAccount : kind === 'AddOwner' ? accountOf(owner, network).account : null);
  const isContract = useKnownContracts();
  // Call
  const targetHex = idHexOf(target);
  const methodName = method.trim();
  const argBytes = parseHexBytes(args);
  const targetError = target.trim() && !targetHex ? t.core.errors.contract : null;
  const methodError = methodName && !methodOk(methodName) ? m.problems.method : null;
  const argsError = argBytes === null ? t.core.errors.hex : argBytes.length > MAX_ARGS_BYTES ? m.problems.argsTooLong : null;
  const items = rows.map((r, i) => {
    const a = rowAssets[i] ?? null;
    const rowMeta = a ? metas.get(a) : undefined;
    const v = rowMeta ? amountOf(r.amount, rowMeta.decimals) : null;
    return a && v !== null ? { asset: a, amount: v } : null;
  });
  const funds = items.every((x): x is FundsItem => x !== null) ? sortedFunds(items as FundsItem[]) : null;
  // SetThreshold: the current threshold until another is chosen
  const n = info.owners.length;
  const chosen = Number(threshold);
  const k = threshold && chosen >= 1 && chosen <= n ? chosen : info.threshold;

  let action: WalletAction | null = null;
  let early: Problem | null = null;
  switch (kind) {
    case 'Transfer':
      if (assetHex && value !== null && toAccount) action = { kind, asset: assetHex, to: toHex(toAccount), amount: value };
      else if (value === 0n) early = 'zeroAmount';
      else if (toAccount && toHex(toAccount) === ZERO_ID) early = 'zeroRecipient';
      break;
    case 'Call':
      if (targetHex && methodName && !methodError && argBytes && !argsError && funds) {
        action = { kind, contract: targetHex, method: methodName, args: argBytes, funds: funds.funds };
      } else if (targetHex === contract) early = 'selfCall';
      else if (targetHex === ZERO_ID) early = 'zeroContract';
      else if (funds?.problem) early = funds.problem;
      else if (items.some((x) => x?.amount === 0n)) early = 'zeroFunds';
      break;
    case 'AddOwner': {
      const account = accountOf(owner, network).account;
      if (account) action = { kind, account: toHex(account) };
      else if (n >= MAX_OWNERS) early = 'tooManyOwners';
      break;
    }
    case 'RemoveOwner':
      if (info.owners.includes(removing)) action = { kind, account: removing };
      else if (n - 1 < info.threshold) early = 'belowThreshold';
      break;
    case 'SetThreshold':
      action = { kind, threshold: k };
      break;
  }
  const problem = action ? checkAction(action, info, contract) : early;
  const same = kind === 'SetThreshold' && k === info.threshold;
  // allowed when proposing: the wallet must hold it only when the proposal is executed
  const more = action !== null && shortfall(action, balances)?.kind === 'balance';
  const sendable = !!wallet && isOwner && action !== null && problem === null && !same && !act.busy;

  const titleOf = (a: WalletAction): string => {
    switch (a.kind) {
      case 'Transfer': return m.titles.Transfer(amountText(m, metas, a.asset, a.amount));
      case 'Call': return m.titles.Call(a.method);
      case 'AddOwner': return m.titles.AddOwner;
      case 'RemoveOwner': return m.titles.RemoveOwner;
      case 'SetThreshold': return m.titles.SetThreshold(m.approvals(a.threshold));
    }
  };
  const detailsOf = (a: WalletAction): Detail[] => {
    const out: Detail[] = [{ label: m.actionLabel, value: actionText(m, a, metas, network, isContract) }];
    if (a.kind === 'Call' && a.args.length > 0) {
      const rest = a.args.length > ARGS_SHOWN ? m.argsMore(a.args.length.toLocaleString()) : '';
      out.push({ label: m.argsLabel, value: `0x${toHex(a.args.slice(0, ARGS_SHOWN))}${rest}` });
    }
    return out;
  };
  const send = () => {
    if (!sendable || !action) return;
    const a = action;
    void act.send('propose', titleOf(a), () => callAction(abi, contract, 'propose', [actionValue(a)]), detailsOf(a)).then((done) => {
      if (!done) return;
      setAmount('');
      setTo('');
      setMethod('');
      setArgs('');
      setRows([]);
      setOwner('');
      setRemoving('');
      setThreshold('');
    });
  };

  const examples = Object.keys(t.core.examples) as (keyof typeof t.core.examples)[];
  const known = conn === null ? [] : examples.flatMap((example) => instancesOf(conn, example)
    .filter((i) => i.id !== contract)
    .map((i) => ({ example, id: i.id })));
  const ownerLabel = (o: string) => {
    const text = accountText(fromHex(o, 32), network, isContract(o));
    const short = text.length > 24 ? `${text.slice(0, 12)}…${text.slice(-8)}` : text;
    return o === me ? `${short} · ${t.core.common.you}` : short;
  };
  const setRow = (key: number, patch: Partial<Row>) => setRows((list) => list.map((r) => (r.key === key ? { ...r, ...patch } : r)));
  const addRow = () => setRows((list) => [...list, { key: rowKey.current++, asset: KSA_HEX, amount: '' }]);

  return <Section title={m.newProposal}>
    <div className="form-grid">
      <div className="segmented multisig-kinds" role="group" aria-label={m.kind}>
        {ACTION_KINDS.map((x) => <button key={x} type="button" className={kind === x ? 'is-active' : ''} aria-pressed={kind === x}
          onClick={() => setKind(x)}>{m.kinds[x]}</button>)}
      </div>
      <p className="field-hint">{m.kindHints[kind]}</p>

      {kind === 'Transfer' && <>
        <AssetSelect label={m.asset} options={assets} value={asset} onChange={setAsset} />
        <UnitsField label={m.amount} unit={meta?.name ?? '…'} decimals={meta?.decimals ?? 0} value={amount} onChange={setAmount} disabled={!meta}
          hint={holds(assetHex)} />
        <AccountField label={m.to} value={to} onChange={setTo} />
      </>}

      {kind === 'Call' && <>
        <Field label={m.contract} htmlFor={ids.target} error={targetError}>
          <input id={ids.target} className="input mono" spellCheck={false} autoCapitalize="off" autoCorrect="off" autoComplete="off"
            value={target} placeholder={t.core.form.idHint} onChange={(e) => setTarget(e.target.value)} />
        </Field>
        {known.length > 0 && <select className="input" aria-label={m.known} value="" onChange={(e) => { if (e.target.value) setTarget(e.target.value); }}>
          <option value="">{m.known}</option>
          {known.map((x) => <option key={`${x.example}:${x.id}`} value={x.id}>{`${t.core.examples[x.example].title} · ${x.id.slice(0, 8)}…`}</option>)}
        </select>}
        <Field label={m.method} htmlFor={ids.method} error={methodError}>
          <input id={ids.method} className="input mono" spellCheck={false} autoCapitalize="off" autoCorrect="off" autoComplete="off"
            value={method} onChange={(e) => setMethod(e.target.value)} />
        </Field>
        <Field label={m.args} htmlFor={ids.args} error={argsError}
          hint={<>{m.argsCount((argBytes?.length ?? 0).toLocaleString())} · {m.argsHint}</>}>
          <textarea id={ids.args} className="input mono" rows={3} spellCheck={false} value={args} placeholder={t.core.form.bytesHint}
            onChange={(e) => setArgs(e.target.value)} />
        </Field>
        <fieldset className="vgroup">
          <legend className="field-label">{m.attach}</legend>
          <span className="field-hint">{m.attachHint}</span>
          {rows.map((r, i) => {
            const a = rowAssets[i] ?? null;
            const rowMeta = a ? metas.get(a) : undefined;
            return <div key={r.key} className="funds-row">
              <AssetSelect label={m.asset} options={assets} value={r.asset} onChange={(v) => setRow(r.key, { asset: v })} />
              <UnitsField label={m.amount} unit={rowMeta?.name ?? '…'} decimals={rowMeta?.decimals ?? 0} value={r.amount} disabled={!rowMeta}
                onChange={(v) => setRow(r.key, { amount: v })} hint={holds(a)} />
              <Button size="sm" variant="ghost" onClick={() => setRows((list) => list.filter((x) => x.key !== r.key))}>{t.core.form.remove}</Button>
            </div>;
          })}
          <div className="method-actions">
            <Button size="sm" disabled={rows.length >= MAX_FUNDS} onClick={addRow}>{m.addAsset}</Button>
            {rows.length >= MAX_FUNDS && <span className="field-hint">{m.problems.tooManyFunds}</span>}
          </div>
        </fieldset>
      </>}

      {kind === 'AddOwner' && <AccountField label={m.newOwner} value={owner} onChange={setOwner} />}

      {kind === 'RemoveOwner' && <Field label={m.ownerToRemove} htmlFor={ids.remove}>
        <select id={ids.remove} className="input mono" value={info.owners.includes(removing) ? removing : ''} onChange={(e) => setRemoving(e.target.value)}>
          <option value="">{m.chooseOwner}</option>
          {info.owners.map((o) => <option key={o} value={o}>{ownerLabel(o)}</option>)}
        </select>
      </Field>}

      {kind === 'SetThreshold' && <Field label={m.newThreshold} htmlFor={ids.threshold}>
        <select id={ids.threshold} className="input" value={String(k)} onChange={(e) => setThreshold(e.target.value)}>
          {Array.from({ length: n }, (_, i) => i + 1).map((i) => <option key={i} value={String(i)}>{m.kOfN(String(i), String(n))}</option>)}
        </select>
      </Field>}

      {more && <Banner tone="warn">{m.moreThanHeld}</Banner>}
      {problem && <p className="field-error">{m.problems[problem]}</p>}
      {same && <p className="field-hint">{m.sameThreshold}</p>}
      <p className="field-hint">{m.proposeHint}</p>
      <div className="method-actions">
        <Button variant="primary" disabled={!sendable} busy={act.busy} onClick={send}>{m.propose}</Button>
      </div>
      {act.error && <Banner tone="danger">{act.error}</Banner>}
    </div>
  </Section>;
}

