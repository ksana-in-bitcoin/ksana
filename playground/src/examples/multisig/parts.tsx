/** Pieces the multisig page's panels share: sentences with values in them, an action or an event in words, the asset picker, sending. */
import { Fragment, createElement, useId, useState, type ReactNode } from 'react';
import { fromHex, type Action as OpAction, type BtcNetwork } from '@ksana/sdk';
import { accountText } from '../../abi/text';
import { KSA_HEX, formatUnits, useAssetMeta, type AssetMeta } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import type { IsContract } from '../../chain/contracts';
import { explorerLink } from '../../chain/deployment';
import type { Activity, Detail } from '../../chain/run';
import { useT, type Messages } from '../../i18n';
import { Id } from '../../ui/ui';
import { AccountText, Amount } from '../../ui/values';
import { useSend } from '../kit';
import { assetHexOf, type FundsItem, type WalletAction, type WalletEvent } from './actions';

type Text = Messages['multisig'];

/** A sentence of the text pack whose values are nodes (an amount, an account): its parts in the language's order */
export function Words({ parts }: { parts: readonly ReactNode[] }) {
  return createElement(Fragment, null, ...parts);
}

/** The same sentence as plain text, its values given as text (for a review's details) */
export function plain(parts: readonly ReactNode[]): string {
  return parts.map((p) => (typeof p === 'string' || typeof p === 'number' ? String(p) : '')).join('');
}

/** An amount as text in the asset's units; base units and the asset's ID only when its decimals cannot be read */
export function amountText(text: Text, metas: ReadonlyMap<string, AssetMeta>, asset: string, value: bigint): string {
  const meta = metas.get(asset);
  return meta ? `${formatUnits(value, meta.decimals)} ${meta.name}` : text.baseUnits(value.toString(), `${asset.slice(0, 8)}…`);
}

function addressOf(hex: string, network: BtcNetwork | undefined, contract: IsContract): string {
  return accountText(fromHex(hex, 32), network, contract(hex));
}

/** An action in words as plain text; `contract` says which of the accounts it names are contracts, written as their IDs */
export function actionText(text: Text, a: WalletAction, metas: ReadonlyMap<string, AssetMeta>, network: BtcNetwork | undefined,
  contract: IsContract = () => false): string {
  switch (a.kind) {
    case 'Transfer':
      return plain(text.action.transfer(amountText(text, metas, a.asset, a.amount), addressOf(a.to, network, contract)));
    case 'Call': {
      const funds = a.funds.map((f) => amountText(text, metas, f.asset, f.amount)).join(text.listSep);
      return plain([...text.action.call(a.method, a.contract, text.argBytes(a.args.length)), ...(a.funds.length ? text.action.attaching(funds) : [])]);
    }
    case 'AddOwner':
      return plain(text.action.addOwner(addressOf(a.account, network, contract)));
    case 'RemoveOwner':
      return plain(text.action.removeOwner(addressOf(a.account, network, contract)));
    case 'SetThreshold':
      return plain(text.action.setThreshold(text.approvals(a.threshold)));
  }
}

/** An account as an address (a contract as its ID), marked when it is the connected one */
export function Who({ account, me }: { account: string; me: string | null }) {
  const t = useT();
  const conn = useConnection();
  return <span className="multisig-who">
    <AccountText value={account} network={conn?.network} />
    {account === me && <span className="pill pill--ok">{t.core.common.you}</span>}
  </span>;
}

function FundsList({ funds }: { funds: readonly FundsItem[] }) {
  const m = useT().multisig;
  return <Words parts={funds.flatMap((f, i) => [...(i ? [m.listSep] : []), <Amount value={f.amount} asset={f.asset} />])} />;
}

/** An action in words, its amounts in their units and its accounts as addresses */
export function ActionWords({ action, me }: { action: WalletAction; me: string | null }) {
  const m = useT().multisig;
  switch (action.kind) {
    case 'Transfer':
      return <Words parts={m.action.transfer(<Amount value={action.amount} asset={action.asset} />, <Who account={action.to} me={me} />)} />;
    case 'Call':
      return <Words parts={[
        ...m.action.call(<code className="mono">{action.method}</code>, <Id value={action.contract} href={explorerLink('contract', action.contract)} />,
          m.argBytes(action.args.length)),
        ...(action.funds.length ? m.action.attaching(<FundsList funds={action.funds} />) : []),
      ]} />;
    case 'AddOwner':
      return <Words parts={m.action.addOwner(<Who account={action.account} me={me} />)} />;
    case 'RemoveOwner':
      return <Words parts={m.action.removeOwner(<Who account={action.account} me={me} />)} />;
    case 'SetThreshold':
      return <Words parts={m.action.setThreshold(<b>{m.approvals(action.threshold)}</b>)} />;
  }
}

/** One of the contract's events as a sentence */
export function EventWords({ event, me }: { event: WalletEvent; me: string | null }) {
  const m = useT().multisig;
  let parts: ReactNode[];
  switch (event.kind) {
    case 'deposit':
      parts = m.events.deposit(<Who account={event.from} me={me} />, <Amount value={event.amount} asset={event.asset} />);
      break;
    case 'propose':
    case 'approve':
    case 'revoke':
    case 'execute':
      parts = m.events[event.kind](<Who account={event.who} me={me} />, event.id.toString());
      break;
    case 'cancel':
      parts = m.events.cancel(event.id.toString());
      break;
    case 'owner_added':
      parts = m.events.ownerAdded(<Who account={event.owner} me={me} />);
      break;
    case 'owner_removed':
      parts = m.events.ownerRemoved(<Who account={event.owner} me={me} />);
      break;
    case 'threshold':
      parts = m.events.threshold(m.approvals(event.threshold));
      break;
  }
  return <p className="multisig-action"><Words parts={parts} /></p>;
}

const OTHER = 'other';

/** An asset: one of `options` by its name, or another asset ID typed in; `value` is the ID chosen or the text typed */
export function AssetSelect({ label, options, value, onChange, disabled }: {
  label: ReactNode; options: readonly string[]; value: string; onChange: (value: string) => void; disabled?: boolean;
}) {
  const t = useT();
  const m = t.multisig;
  const id = useId();
  const [typing, setTyping] = useState(() => value !== '' && !options.includes(value));
  const listed = !typing && options.includes(value);
  const bad = typing && value.trim() !== '' && assetHexOf(value) === null;
  return <div className="vfield">
    <label className="field-label" htmlFor={id}>{label}</label>
    <select id={id} className="input" disabled={disabled} value={typing ? OTHER : listed ? value : ''}
      onChange={(e) => {
        const next = e.target.value;
        setTyping(next === OTHER);
        onChange(next === OTHER ? '' : next);
      }}>
      {!typing && !listed && <option value="">{m.chooseAsset}</option>}
      {options.map((a) => <AssetOption key={a} asset={a} />)}
      <option value={OTHER}>{m.otherAsset}</option>
    </select>
    {typing && <input className="input mono" aria-label={m.otherAsset} spellCheck={false} autoCapitalize="off" autoCorrect="off" autoComplete="off"
      disabled={disabled} value={value} placeholder={t.core.form.idHint} onChange={(e) => onChange(e.target.value)} />}
    {bad && <span className="field-error">{m.badAsset}</span>}
  </div>;
}

function AssetOption({ asset }: { asset: string }) {
  const conn = useConnection();
  const meta = useAssetMeta(conn, asset);
  const short = `${asset.slice(0, 8)}…`;
  return <option value={asset}>{asset === KSA_HEX ? 'KSA' : meta && meta.name !== short ? `${meta.name} · ${short}` : short}</option>;
}

/**
 * One panel's operations through the review, like `useSend`, and which of them is running; an operation that cannot be built
 * (a contract deployed from edited code whose ABI lacks the method) shows why instead.
 */
export function useAct() {
  const sender = useSend();
  const [failed, setFailed] = useState<string | null>(null);
  const [which, setWhich] = useState<string | null>(null);
  const send = async (tag: string, title: string, build: () => OpAction, details?: Detail[]): Promise<Activity | null> => {
    let action: OpAction;
    try {
      action = build();
    } catch (e) {
      setFailed(e instanceof Error ? e.message : String(e));
      return null;
    }
    setFailed(null);
    setWhich(tag);
    try {
      return await sender.send({ title, action, ...(details ? { details } : {}) });
    } finally {
      setWhich(null);
    }
  };
  return { send, busy: sender.busy, which: sender.busy ? which : null, error: failed ?? sender.error };
}
