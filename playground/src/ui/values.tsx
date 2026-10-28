/** Forms and views generated from ABI types, and the inputs shared by the example pages (accounts, assets, amounts, funds). */
import { useId, type ReactNode } from 'react';
import { fromHex, parseAccount, toHex, type BtcNetwork, type Funds } from '@ksana/sdk';
import { type Draft, accountText, draftOf, emptyDraft, formatValue, serialOf, withVariant } from '../abi/text';
import { type Abi, type AbiType, type Value, isEnumValue, typeName } from '../abi/types';
import { useAccount } from '../chain/account';
import { KSA_HEX, formatUnits, parseUnits, useAssetMeta } from '../chain/assets';
import { useConnection } from '../chain/connection';
import { useIsContract } from '../chain/contracts';
import { explorerLink } from '../chain/deployment';
import { useWallet } from '../chain/wallet';
import { useT } from '../i18n';
import { errorText } from './errors';
import { Button, Id } from './ui';

function inputKind(type: AbiType): 'int' | 'text' | 'hex' | 'account' | 'asset' | 'token' {
  if (type.kind === 'int') return 'int';
  if (type.kind === 'id' && type.name === 'Account') return 'account';
  if (type.kind === 'id' && type.name === 'AssetId') return 'asset';
  if (type.kind === 'id' && type.name === 'TokenId') return 'token';
  if (type.kind === 'string') return 'text';
  return 'hex';
}

export interface ValueEditorProps {
  abi: Abi;
  type: AbiType;
  draft: Draft;
  onChange: (draft: Draft) => void;
  label: ReactNode;
  disabled?: boolean;
  depth?: number;
}

/** Buttons that fill an asset ID field with an asset the connected wallet holds besides KSA (which has its own): its asset ID, never a contract's */
function WalletAssets({ onPick, disabled }: { onPick: (hex: string) => void; disabled: boolean }) {
  const m = useT().core.form;
  const account = useAccount();
  const held = account ? [...account.balances].filter(([hex, amount]) => hex !== KSA_HEX && amount > 0n).slice(0, 4) : [];
  if (held.length === 0) return null;
  return <div className="method-actions" role="group" aria-label={m.yourAssets}>
    <span className="field-hint">{m.yourAssets}</span>
    {held.map(([hex]) => <Button key={hex} size="sm" disabled={disabled} onClick={() => onPick(hex)}><AssetName asset={hex} /></Button>)}
  </div>;
}

/** An editor for one value of any ABI type (recursive for options, lists, tuples, structs and enums) */
export function ValueEditor({ abi, type, draft, onChange, label, disabled = false, depth = 0 }: ValueEditorProps) {
  const m = useT().core.form;
  const id = useId();
  const wallet = useWallet();
  const hint = <span className="type-name">{typeName(type)}</span>;
  switch (type.kind) {
    case 'int':
    case 'string':
    case 'bytes':
    case 'fixed':
    case 'id': {
      const kind = inputKind(type);
      const multiline = type.kind === 'bytes';
      const placeholder = kind === 'account' ? m.accountHint : kind === 'asset' ? m.assetHint : kind === 'token' ? m.tokenIdHint
        : kind === 'hex' ? (type.kind === 'bytes' ? m.bytesHint : m.idHint) : typeName(type);
      return <div className="vfield">
        <label className="field-label" htmlFor={id}>{label} {hint}</label>
        <div className="input-row">
          {multiline
            ? <textarea id={id} className="input mono" rows={2} spellCheck={false} disabled={disabled} value={draft.text} placeholder={placeholder}
              onChange={(e) => onChange({ ...draft, text: e.target.value })} />
            : <input id={id} className={`input${kind === 'text' ? '' : ' mono'}`} spellCheck={false} autoCapitalize="off" autoCorrect="off" disabled={disabled}
              inputMode={kind === 'int' ? 'numeric' : undefined} value={draft.text} placeholder={placeholder}
              onChange={(e) => onChange({ ...draft, text: e.target.value })} />}
          {kind === 'account' && wallet && <Button size="sm" disabled={disabled} onClick={() => onChange({ ...draft, text: wallet.account.address })}>{m.me}</Button>}
          {kind === 'asset' && <Button size="sm" disabled={disabled} onClick={() => onChange({ ...draft, text: 'KSA' })}>{m.ksa}</Button>}
        </div>
        {kind === 'asset' && <WalletAssets disabled={disabled} onPick={(hex) => onChange({ ...draft, text: hex })} />}
      </div>;
    }
    case 'bool':
      return <label className="vfield check"><input type="checkbox" disabled={disabled} checked={draft.on} onChange={(e) => onChange({ ...draft, on: e.target.checked })} />
        <span>{label} {hint}</span></label>;
    case 'option': {
      const inner = draft.items[0] ?? emptyDraft(abi, type.inner);
      return <div className="vgroup">
        <label className="check"><input type="checkbox" disabled={disabled} checked={draft.on} onChange={(e) => onChange({ ...draft, on: e.target.checked, items: [inner] })} />
          <span>{label} {hint}{!draft.on && <span className="muted"> · {m.none}</span>}</span></label>
        {draft.on && <ValueEditor abi={abi} type={type.inner} draft={inner} disabled={disabled} depth={depth + 1} label={m.some}
          onChange={(d) => onChange({ ...draft, items: [d] })} />}
      </div>;
    }
    case 'vec':
      return <fieldset className="vgroup" disabled={disabled}>
        <legend className="field-label">{label} {hint}</legend>
        {draft.items.map((item, i) => <div key={i} className="vitem">
          <ValueEditor abi={abi} type={type.inner} draft={item} depth={depth + 1} label={`#${i + 1}`}
            onChange={(d) => onChange({ ...draft, items: draft.items.map((x, j) => (j === i ? d : x)) })} />
          <Button size="sm" variant="ghost" onClick={() => onChange({ ...draft, items: draft.items.filter((_, j) => j !== i) })}>{m.remove}</Button>
        </div>)}
        <Button size="sm" onClick={() => onChange({ ...draft, items: [...draft.items, emptyDraft(abi, type.inner)] })}>{m.add}</Button>
      </fieldset>;
    case 'tuple':
      if (type.items.length === 0) return null;
      return <fieldset className="vgroup" disabled={disabled}>
        <legend className="field-label">{label} {hint}</legend>
        {type.items.map((t, i) => <ValueEditor key={i} abi={abi} type={t} draft={draft.items[i] ?? emptyDraft(abi, t)} depth={depth + 1} label={`.${i}`}
          onChange={(d) => onChange({ ...draft, items: type.items.map((tt, j) => (j === i ? d : draft.items[j] ?? emptyDraft(abi, tt))) })} />)}
      </fieldset>;
    case 'struct': {
      const fields = abi.structs.get(type.name)?.fields ?? [];
      return <fieldset className="vgroup" disabled={disabled}>
        <legend className="field-label">{label} {hint}</legend>
        {fields.map((f, i) => <ValueEditor key={f.name} abi={abi} type={f.type} draft={draft.items[i] ?? emptyDraft(abi, f.type)} depth={depth + 1} label={f.name}
          onChange={(d) => onChange({ ...draft, items: fields.map((ff, j) => (j === i ? d : draft.items[j] ?? emptyDraft(abi, ff.type))) })} />)}
      </fieldset>;
    }
    case 'enum': {
      const def = abi.enums.get(type.name);
      const variant = def?.variants[draft.variant];
      return <fieldset className="vgroup" disabled={disabled}>
        <legend className="field-label">{label} {hint}</legend>
        <select className="input" aria-label={m.variant} value={draft.variant} onChange={(e) => onChange(withVariant(abi, type, draft, Number(e.target.value)))}>
          {def?.variants.map((v, i) => <option key={v.name} value={i}>{v.name}</option>)}
        </select>
        {variant?.fields.map((t, i) => <ValueEditor key={`${draft.variant}.${i}`} abi={abi} type={t} draft={draft.items[i] ?? emptyDraft(abi, t)} depth={depth + 1}
          label={`${variant.name}.${i}`}
          onChange={(d) => onChange({ ...draft, items: variant.fields.map((tt, j) => (j === i ? d : draft.items[j] ?? emptyDraft(abi, tt))) })} />)}
      </fieldset>;
    }
  }
}

export { draftOf, emptyDraft };

/**
 * An account, linked to the explorer: a contract by its ID, as the explorer writes it, once the node says it is one; any other
 * account as its address on the node's network when it is a key
 */
export function AccountText({ value, network, chars = 8 }: { value: Uint8Array | string; network?: BtcNetwork | undefined; chars?: number }) {
  const hex = typeof value === 'string' ? value : toHex(value);
  const bytes = typeof value === 'string' ? fromHex(value, 32) : value;
  const contract = useIsContract(hex);
  return <Id value={accountText(bytes, network, contract)} chars={chars} href={explorerLink(contract ? 'contract' : 'account', hex)} />;
}

/** A decoded value, laid out by its type */
export function ValueView({ abi, type, value }: { abi: Abi; type: AbiType | null; value: Value }) {
  const conn = useConnection();
  const network = conn?.network;
  if (type === null) return <span className="muted">()</span>;
  switch (type.kind) {
    case 'int':
      return <span className="mono">{typeof value === 'bigint' ? value.toString() : '?'}</span>;
    case 'bool':
      return <span className="mono">{value === true ? 'true' : 'false'}</span>;
    case 'string':
      return <span className="break">{typeof value === 'string' ? value : '?'}</span>;
    case 'bytes':
    case 'fixed':
      return <span className="mono break">{value instanceof Uint8Array ? `0x${toHex(value)}` : '?'}</span>;
    case 'id':
      if (!(value instanceof Uint8Array)) return <span>?</span>;
      if (type.name === 'AssetId' && toHex(value) === KSA_HEX) return <span className="mono">KSA</span>;
      if (type.name === 'Account') return <AccountText value={value} network={network} />;
      if (type.name === 'TokenId' && serialOf(value) !== null) return <span className="mono" title={toHex(value)}>#{serialOf(value)!.toString()}</span>;
      return <Id value={toHex(value)} href={type.name === 'ContractId' ? explorerLink('contract', toHex(value)) : null} />;
    case 'option':
      return value === null ? <span className="muted">None</span> : <ValueView abi={abi} type={type.inner} value={value} />;
    case 'vec':
      if (!Array.isArray(value)) return <span>?</span>;
      if (value.length === 0) return <span className="muted">[]</span>;
      return <ol className="vlist">{value.map((v, i) => <li key={i}><ValueView abi={abi} type={type.inner} value={v} /></li>)}</ol>;
    case 'tuple':
      if (!Array.isArray(value)) return <span>?</span>;
      if (type.items.length === 0) return <span className="muted">()</span>;
      return <dl className="vrows">{type.items.map((t, i) => <div key={i}><dt>.{i}</dt><dd><ValueView abi={abi} type={t} value={value[i] ?? null} /></dd></div>)}</dl>;
    case 'struct': {
      const def = abi.structs.get(type.name);
      if (!def || !value || typeof value !== 'object' || Array.isArray(value) || value instanceof Uint8Array || isEnumValue(value)) return <span>?</span>;
      const record = value as { [field: string]: Value };
      return <dl className="vrows">{def.fields.map((f) => <div key={f.name}><dt>{f.name}</dt><dd><ValueView abi={abi} type={f.type} value={record[f.name] ?? null} /></dd></div>)}</dl>;
    }
    case 'enum': {
      if (value === null || !isEnumValue(value)) return <span>?</span>;
      const variant = abi.enums.get(type.name)?.variants.find((v) => v.name === value.variant);
      if (!variant || variant.fields.length === 0) return <span className="mono">{value.variant}</span>;
      return <div className="venum"><span className="mono">{value.variant}</span>
        <dl className="vrows">{variant.fields.map((t, i) => <div key={i}><dt>.{i}</dt><dd><ValueView abi={abi} type={t} value={value.values[i] ?? null} /></dd></div>)}</dl></div>;
    }
  }
}

/** A value as one line (for the review and short summaries) */
export function valueLine(abi: Abi, type: AbiType, value: Value, network?: BtcNetwork): string {
  return formatValue(abi, type, value, { network });
}

/** An amount of an asset in its own units, with its name (… until its decimals are known) */
export function Amount({ value, asset, maxFraction }: { value: bigint; asset: string; maxFraction?: number }) {
  const conn = useConnection();
  const meta = useAssetMeta(conn, asset);
  if (!meta) return <span className="mono muted" title={value.toString()}>…</span>;
  return <span className="amount"><span className="mono">{formatUnits(value, meta.decimals, maxFraction)}</span> <span className="unit">{meta.name}</span></span>;
}

/** The name of an asset (KSA, or the name its issuer gave it) */
export function AssetName({ asset }: { asset: string }) {
  const conn = useConnection();
  const meta = useAssetMeta(conn, asset);
  return <span className="unit">{meta?.name ?? `${asset.slice(0, 8)}…`}</span>;
}

/** What Max keeps back of the account's own KSA for the operation's fee (signet fees are far below it) */
export const KSA_FEE_RESERVE = 1_000_000n;

/**
 * An amount typed in the asset's units; shows the connected account's balance and a Max button. Max of the account's own KSA
 * leaves `KSA_FEE_RESERVE` for the fee; `max` replaces the balance (the page's own limit, kept whole). `error` is the page's own
 * reason against the amount (more than a limit), shown when the text is an amount.
 */
export function AmountField({ label, asset, value, onChange, disabled, max, hint, error: outside }: {
  label: ReactNode; asset: string; value: string; onChange: (v: string) => void; disabled?: boolean; max?: bigint | null; hint?: ReactNode;
  error?: ReactNode;
}) {
  const t = useT();
  const m = t.core;
  const id = useId();
  const conn = useConnection();
  const meta = useAssetMeta(conn, asset);
  const account = useAccount();
  const balance = max === undefined ? account?.balances.get(asset) ?? 0n : max;
  const reserve = max === undefined && asset === KSA_HEX ? KSA_FEE_RESERVE : 0n;
  const most = balance === null ? null : balance > reserve ? balance - reserve : 0n;
  const decimals = meta?.decimals ?? 0;
  let error: ReactNode = null;
  if (value.trim() && meta) {
    try {
      parseUnits(value, decimals);
      error = outside ?? null;
    } catch (e) {
      error = errorText(e, t);
    }
  }
  return <div className="vfield">
    <label className="field-label" htmlFor={id}>{label}</label>
    <div className="input-row">
      <input id={id} className="input mono" inputMode="decimal" autoComplete="off" disabled={disabled} value={value} placeholder="0" onChange={(e) => onChange(e.target.value)}
        aria-invalid={error ? true : undefined} />
      <span className="unit">{meta?.name ?? '…'}</span>
      {most !== null && meta && <Button size="sm" variant="ghost" disabled={disabled} title={reserve > 0n ? m.form.keepsFee(formatUnits(reserve, decimals)) : undefined}
        onClick={() => onChange(formatUnits(most, decimals).replace(/,/g, ''))}>{m.common.max}</Button>}
    </div>
    {error ? <span className="field-error">{error}</span> : <span className="field-hint">
      {balance !== null && meta ? <>{m.wallet.balance}: {formatUnits(balance, decimals)} {meta.name}</> : null}{hint ? <> · {hint}</> : null}
    </span>}
  </div>;
}

/** An amount typed in units of known decimals (a token whose page read them), with a Max button when there is a most */
export function UnitsField({ label, unit, value, onChange, decimals, max, disabled, hint }: {
  label: ReactNode; unit: string; value: string; onChange: (v: string) => void; decimals: number; max?: bigint | null; disabled?: boolean; hint?: ReactNode;
}) {
  const t = useT();
  const m = t.core;
  const id = useId();
  let error: string | null = null;
  if (value.trim()) {
    try {
      const v = parseUnits(value, decimals);
      if (max !== undefined && max !== null && v > max) error = `\u2264 ${formatUnits(max, decimals)} ${unit}`;
    } catch (e) {
      error = errorText(e, t);
    }
  }
  return <div className="vfield">
    <label className="field-label" htmlFor={id}>{label}</label>
    <div className="input-row">
      <input id={id} className="input mono" inputMode="decimal" autoComplete="off" disabled={disabled} value={value} placeholder="0" onChange={(e) => onChange(e.target.value)} />
      <span className="unit">{unit}</span>
      {max !== undefined && max !== null && max > 0n &&
        <Button size="sm" variant="ghost" disabled={disabled} onClick={() => onChange(formatUnits(max, decimals).replace(/,/g, ''))}>{m.common.max}</Button>}
    </div>
    {error ? <span className="field-error">{error}</span> : hint ? <span className="field-hint">{hint}</span> : null}
  </div>;
}

/** The amount typed, in base units; null when it is empty or not a valid amount for the asset */
export function amountOf(text: string, decimals: number | undefined): bigint | null {
  if (decimals === undefined || !text.trim()) return null;
  try {
    return parseUnits(text, decimals);
  } catch {
    return null;
  }
}

export interface FundsDraft {
  asset: string;
  amount: string;
}

/** Funds to attach: rows of an asset (KSA or an asset ID) and an amount in its units */
export function FundsEditor({ rows, onChange, disabled }: { rows: FundsDraft[]; onChange: (rows: FundsDraft[]) => void; disabled?: boolean }) {
  const m = useT().core;
  return <fieldset className="vgroup funds" disabled={disabled}>
    <legend className="field-label">{m.methods.attach}</legend>
    {rows.map((row, i) => <FundsRow key={i} row={row} onChange={(r) => onChange(rows.map((x, j) => (j === i ? r : x)))}
      onRemove={() => onChange(rows.filter((_, j) => j !== i))} />)}
    <Button size="sm" onClick={() => onChange([...rows, { asset: 'KSA', amount: '' }])}>{m.methods.addFunds}</Button>
  </fieldset>;
}

function assetHexOf(text: string): string | null {
  if (/^\s*ksa\s*$/i.test(text)) return KSA_HEX;
  const t = text.trim().toLowerCase().replace(/^0x/, '');
  return /^[0-9a-f]{64}$/.test(t) ? t : null;
}

function FundsRow({ row, onChange, onRemove }: { row: FundsDraft; onChange: (r: FundsDraft) => void; onRemove: () => void }) {
  const m = useT().core;
  const asset = assetHexOf(row.asset);
  return <div className="funds-row">
    <div className="vfield"><label className="field-label">{m.methods.asset}</label>
      <input className="input mono" spellCheck={false} value={row.asset} placeholder={m.form.assetHint} onChange={(e) => onChange({ ...row, asset: e.target.value })} /></div>
    {asset ? <AmountField label={m.methods.amount} asset={asset} value={row.amount} onChange={(amount) => onChange({ ...row, amount })} />
      : <div className="vfield"><label className="field-label">{m.methods.amount}</label><input className="input" disabled value="" /></div>}
    <Button size="sm" variant="ghost" onClick={onRemove}>{m.form.remove}</Button>
  </div>;
}

/** The funds the rows stand for (asset decimals known); throws on a row that is not complete */
export async function fundsOf(rows: FundsDraft[], decimalsOf: (asset: string) => Promise<number>): Promise<Funds[]> {
  const out: Funds[] = [];
  for (const row of rows) {
    const asset = assetHexOf(row.asset);
    if (!asset) throw new Error(`asset: ${row.asset || '?'}`);
    const amount = parseUnits(row.amount, await decimalsOf(asset));
    if (amount > 0n) out.push({ asset: fromHex(asset, 32), amount });
  }
  return out;
}

/** The account typed (a Taproot address of the node's network, or 64 hex digits), or the SDK's reason it is not one */
export function accountOf(text: string, network: BtcNetwork | undefined): { account: Uint8Array | null; error: string | null } {
  if (!text.trim()) return { account: null, error: null };
  try {
    return { account: parseAccount(text, network), error: null };
  } catch (e) {
    return { account: null, error: e instanceof Error ? e.message : String(e) };
  }
}

/** An account typed as a Taproot address or hex, with a button for the connected wallet's own */
export function AccountField({ label, value, onChange, disabled, hint }: {
  label: ReactNode; value: string; onChange: (v: string) => void; disabled?: boolean; hint?: ReactNode;
}) {
  const m = useT().core.form;
  const id = useId();
  const wallet = useWallet();
  const conn = useConnection();
  const { error } = accountOf(value, conn?.network);
  return <div className="vfield">
    <label className="field-label" htmlFor={id}>{label}</label>
    <div className="input-row">
      <input id={id} className="input mono" spellCheck={false} autoCapitalize="off" autoCorrect="off" disabled={disabled} value={value}
        placeholder={m.accountHint} onChange={(e) => onChange(e.target.value)} />
      {wallet && <Button size="sm" disabled={disabled} onClick={() => onChange(wallet.account.address)}>{m.me}</Button>}
    </div>
    {error ? <span className="field-error" title={error}>{m.notAccount}</span> : hint ? <span className="field-hint">{hint}</span> : null}
  </div>;
}

/** Whether two accounts (bytes or hex) are the same */
export function sameAccount(a: Uint8Array | string | null | undefined, b: Uint8Array | string | null | undefined): boolean {
  if (!a || !b) return false;
  return (typeof a === 'string' ? a : toHex(a)) === (typeof b === 'string' ? b : toHex(b));
}
