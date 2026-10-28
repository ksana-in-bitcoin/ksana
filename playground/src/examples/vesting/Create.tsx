/** Creating a schedule: the asset and amount attached to `create`, the beneficiary, the start, the cliff and the duration, and a preview. */
import { useId, useMemo, useState } from 'react';
import { fromHex } from '@ksana/sdk';
import { decodeReturn } from '../../abi/codec';
import type { Abi } from '../../abi/types';
import { useAccount } from '../../chain/account';
import { KSA_HEX, formatUnits, useAssetMeta, type AssetMeta } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { useWallet } from '../../chain/wallet';
import { useLocale, useT } from '../../i18n';
import { Banner, Button, Field, Section } from '../../ui/ui';
import { AccountField, AmountField, accountOf, amountOf, sameAccount } from '../../ui/values';
import { callAction, methodOf, useSend } from '../kit';
import { durationText } from './Card';
import { VestingChart } from './Chart';
import { useAssetMetas } from './reads';
import {
  HOUR, PRESET_ORDER, checkTerms, choiceSeconds, dateLocale, formatDateTime, formatTick, localInputValue, parseLocalDateTime, percentText, previewOffsets,
  vestedAt, type Preset, type Terms, type Unit,
} from './schedule';

const UNIT_ORDER: readonly Unit[] = ['minutes', 'hours', 'days'];
const CLIFF_CHOICES = PRESET_ORDER;
/** A schedule needs a duration of at least a second, so there is no "none" */
const DURATION_CHOICES = PRESET_ORDER.filter((p) => p !== 'none');

/** An asset ID typed (64 hex digits, or KSA), as hex; null otherwise */
function assetHexOf(text: string): string | null {
  if (/^\s*ksa\s*$/i.test(text)) return KSA_HEX;
  const t = text.trim().toLowerCase().replace(/^0x/, '');
  return /^[0-9a-f]{64}$/.test(t) ? t : null;
}

/** The ID `create` returned, from the receipt's output */
function createdId(abi: Abi, output: Uint8Array | undefined): bigint | null {
  if (!output) return null;
  try {
    const value = decodeReturn(abi, methodOf(abi, 'create').returns, output);
    return typeof value === 'bigint' ? value : null;
  } catch {
    return null;
  }
}

function timeZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone ?? '';
  } catch {
    return '';
  }
}

/** A choice of buttons, one of them pressed */
function Choice<T extends string>({ label, options, value, onChange }: {
  label: string; options: readonly { value: T; label: string }[]; value: T; onChange: (v: T) => void;
}) {
  return <div className="segmented vesting-choice" role="radiogroup" aria-label={label}>
    {options.map((o) => <button key={o.value} type="button" role="radio" aria-checked={o.value === value}
      className={o.value === value ? 'is-active' : ''} onClick={() => onChange(o.value)}>{o.label}</button>)}
  </div>;
}

/** A length of time: a preset, or a whole number of minutes, hours or days */
function LengthField({ label, hint, error, choices, choice, onChoice, custom, onCustom, unit, onUnit }: {
  label: string; hint: string; error: string | null; choices: readonly Preset[]; choice: Preset; onChoice: (p: Preset) => void;
  custom: string; onCustom: (v: string) => void; unit: Unit; onUnit: (u: Unit) => void;
}) {
  const m = useT().vesting;
  return <fieldset className="vesting-field">
    <legend className="field-label">{label}</legend>
    <Choice label={label} value={choice} onChange={onChoice} options={choices.map((c) => ({ value: c, label: m.presets[c] }))} />
    {choice === 'custom' && <div className="input-row">
      <input className="input mono" inputMode="numeric" autoComplete="off" placeholder="1" aria-label={m.howMany(label)} value={custom}
        onChange={(e) => onCustom(e.target.value)} />
      <select className="input" aria-label={m.unit} value={unit} onChange={(e) => onUnit(e.target.value as Unit)}>
        {UNIT_ORDER.map((u) => <option key={u} value={u}>{m.unitNames[u]}</option>)}
      </select>
    </div>}
    {error ? <span className="field-error">{error}</span> : <span className="field-hint">{hint}</span>}
  </fieldset>;
}

export function CreatePanel({ contract, abi, now, onCreated }: {
  contract: string; abi: Abi;
  /** The chain's time */
  now: bigint;
  onCreated: (id: bigint) => void;
}) {
  const t = useT();
  const m = t.vesting;
  const conn = useConnection();
  const wallet = useWallet();
  const account = useAccount();
  const locale = dateLocale(useLocale());
  const sender = useSend();
  const ids = useId();

  const [choice, setChoice] = useState('ksa');
  const [typedAsset, setTypedAsset] = useState('');
  const [amount, setAmount] = useState('');
  const [beneficiary, setBeneficiary] = useState('');
  const [startMode, setStartMode] = useState<'now' | 'at'>('now');
  const [startText, setStartText] = useState('');
  const [cliffChoice, setCliffChoice] = useState<Preset>('none');
  const [cliffCustom, setCliffCustom] = useState('');
  const [cliffUnit, setCliffUnit] = useState<Unit>('days');
  const [durationChoice, setDurationChoice] = useState<Preset>('hour');
  const [durationCustom, setDurationCustom] = useState('');
  const [durationUnit, setDurationUnit] = useState<Unit>('days');
  const [revocable, setRevocable] = useState(true);
  const [created, setCreated] = useState<bigint | null>(null);

  // The assets of the wallet besides KSA, and one chosen before that it no longer holds
  const held = useMemo(() => (account ? [...account.balances.entries()].filter(([hex, v]) => hex !== KSA_HEX && v > 0n).map(([hex]) => hex).sort() : []), [account]);
  const listed = choice !== 'ksa' && choice !== 'other' && !held.includes(choice) ? [...held, choice] : held;
  const names = useAssetMetas(listed);
  const assetHex = choice === 'ksa' ? KSA_HEX : choice === 'other' ? assetHexOf(typedAsset) : choice;
  const meta = useAssetMeta(conn, assetHex);
  const unknownAsset = !!meta && meta.id !== KSA_HEX && meta.totalSupply === null;
  const balance = account && assetHex ? account.balances.get(assetHex) ?? 0n : null;

  const value = amountOf(amount, meta?.decimals);
  let amountError: string | null = null;
  if (value === 0n) amountError = m.zeroAmount;
  else if (value !== null && balance !== null && value > balance) amountError = m.overBalance;

  const target = accountOf(beneficiary, conn?.network).account;
  let beneficiaryError: string | null = null;
  if (target && target.every((b) => b === 0)) beneficiaryError = m.zeroBeneficiary;
  else if (target && sameAccount(target, contract)) beneficiaryError = m.selfBeneficiary;

  const startAt = startMode === 'now' ? null : parseLocalDateTime(startText);
  const cliff = choiceSeconds(cliffChoice, cliffCustom, cliffUnit);
  const duration = choiceSeconds(durationChoice, durationCustom, durationUnit);
  // Without a valid start yet, the cliff and the duration are checked on their own
  const problem = cliff !== null && duration !== null ? checkTerms({ start: startAt, cliff, duration }, now) : null;
  const startError = startMode === 'at' && startAt === null ? m.problems.noStart
    : problem === 'startEarly' || problem === 'startFar' || problem === 'overflow' ? m.problems[problem] : null;
  // What the button waits for while no field says what is wrong
  const noAmount = !amount.trim();
  const noTarget = !beneficiary.trim();
  const noCliff = cliffChoice === 'custom' && !cliffCustom.trim();
  const noDuration = durationChoice === 'custom' && !durationCustom.trim();
  const missing = !wallet ? null
    : choice === 'other' && !typedAsset.trim() ? m.fillIn.asset
    : noAmount && noTarget ? m.fillIn.both
    : noAmount ? m.fillIn.amount
    : noTarget ? m.fillIn.beneficiary
    : noCliff ? m.fillIn.cliff
    : noDuration ? m.fillIn.duration
    : null;
  const ready = !!wallet && !!meta && !unknownAsset && value !== null && value > 0n && !amountError && !!target && !beneficiaryError &&
    cliff !== null && duration !== null && (startMode === 'now' || startAt !== null) && problem === null;

  const pickStart = (mode: 'now' | 'at') => {
    setStartMode(mode);
    // The next full hour, to start from
    if (mode === 'at' && !parseLocalDateTime(startText)) setStartText(localInputValue(now + HOUR - (now % HOUR)));
  };

  const create = () => {
    if (!ready || !meta || value === null || !target || cliff === null || duration === null || !assetHex) return;
    const amountText = `${formatUnits(value, meta.decimals)} ${meta.name}`;
    setCreated(null);
    void sender.send({
      title: m.createTitle(amountText),
      action: callAction(abi, contract, 'create', [target, startAt ?? 0n, cliff, duration, revocable], [{ asset: fromHex(assetHex, 32), amount: value }]),
      details: [
        // The review lists the attached amount itself
        { label: m.beneficiary, value: beneficiary.trim() },
        { label: m.start, value: startAt === null ? m.startDetail : formatDateTime(startAt, locale) },
        { label: m.cliff, value: durationText(m, cliff) },
        { label: m.duration, value: durationText(m, duration) },
        { label: m.revocable, value: revocable ? t.core.common.yes : t.core.common.no },
      ],
    }).then((activity) => {
      if (!activity) return;
      setAmount('');
      const id = createdId(abi, activity.status?.receipt?.output ?? undefined);
      setCreated(id);
      if (id !== null) onCreated(id);
    });
  };

  const assetOptions = [
    { value: 'ksa', label: 'KSA' },
    ...listed.map((hex) => ({ value: hex, label: names.get(hex)?.name ?? `${hex.slice(0, 8)}…` })),
    { value: 'other', label: m.otherAsset },
  ];
  const previewStart = startMode === 'now' ? now : startAt;
  return <Section title={m.create}>
    <p className="field-hint">{m.createHint}</p>
    <div className="form-grid">
      <div className="form-row">
        <Field label={m.asset} htmlFor={`${ids}-asset`}>
          <select id={`${ids}-asset`} className="input" value={choice} onChange={(e) => setChoice(e.target.value)}>
            {assetOptions.map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
          </select>
        </Field>
        {choice === 'other' && <Field label={m.assetId} htmlFor={`${ids}-id`} hint={m.assetIdHint}
          error={typedAsset.trim() && !assetHexOf(typedAsset) ? m.badAssetId : unknownAsset ? m.unknownAsset : null}>
          <input id={`${ids}-id`} className="input mono" spellCheck={false} autoCapitalize="off" autoCorrect="off" value={typedAsset}
            placeholder={t.core.form.idHint} onChange={(e) => setTypedAsset(e.target.value)} />
        </Field>}
      </div>
      {assetHex
        ? <div className="vfield">
          <AmountField label={m.amount} asset={assetHex} value={amount} onChange={setAmount} />
          {amountError && <span className="field-error">{amountError}</span>}
        </div>
        : <Field label={m.amount}><input className="input" disabled value="" aria-label={m.amount} /></Field>}
      <div className="vfield">
        <AccountField label={m.beneficiary} value={beneficiary} onChange={setBeneficiary} hint={m.beneficiaryHint} />
        {beneficiaryError && <span className="field-error">{beneficiaryError}</span>}
      </div>
      <fieldset className="vesting-field">
        <legend className="field-label">{m.start}</legend>
        <Choice label={m.start} value={startMode} onChange={pickStart}
          options={[{ value: 'now', label: m.startNow }, { value: 'at', label: m.startAt }]} />
        {startMode === 'at' && <input id={`${ids}-start`} type="datetime-local" className="input mono vesting-when" aria-label={m.startAt}
          value={startText} onChange={(e) => setStartText(e.target.value)} />}
        {startError ? <span className="field-error">{startError}</span>
          : <span className="field-hint">{startMode === 'now' ? m.startNowHint : m.startAtHint(timeZone())}</span>}
      </fieldset>
      <LengthField label={m.cliff} hint={m.cliffHint} choices={CLIFF_CHOICES} choice={cliffChoice} onChoice={setCliffChoice}
        custom={cliffCustom} onCustom={setCliffCustom} unit={cliffUnit} onUnit={setCliffUnit}
        error={cliff === null && !noCliff ? m.badNumber : problem === 'cliff' ? m.problems.cliff : null} />
      <LengthField label={m.duration} hint={m.durationHint} choices={DURATION_CHOICES} choice={durationChoice} onChoice={setDurationChoice}
        custom={durationCustom} onCustom={setDurationCustom} unit={durationUnit} onUnit={setDurationUnit}
        error={duration === null && !noDuration ? m.badNumber : problem === 'duration' ? m.problems.duration : null} />
      <label className="check">
        <input type="checkbox" checked={revocable} onChange={(e) => setRevocable(e.target.checked)} />
        <span>{m.revocable}<span className="field-hint vesting-check-hint">{revocable ? m.revocableOn : m.revocableOff}</span></span>
      </label>
      {previewStart !== null && cliff !== null && duration !== null && problem !== 'duration' && problem !== 'cliff' &&
        <Preview start={previewStart} approximate={startMode === 'now'} cliff={cliff} duration={duration} total={value && value > 0n ? value : null}
          meta={meta} now={now} />}
      <div className="method-actions">
        <Button variant="primary" disabled={!ready || sender.busy} busy={sender.busy} onClick={create}>{m.createButton}</Button>
      </div>
      {!wallet ? <p className="field-hint">{t.core.errors.connectFirst}</p> : missing && <p className="field-hint">{missing}</p>}
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
      {created !== null && <Banner tone="ok">{m.createdOk(created.toString())}</Banner>}
    </div>
  </Section>;
}

/** The schedule as it would vest: drawn over time, and the vested amount at the cliff, the quarters and the end */
function Preview({ start, approximate, cliff, duration, total, meta, now }: {
  start: bigint; approximate: boolean; cliff: bigint; duration: bigint; total: bigint | null; meta: AssetMeta | null; now: bigint;
}) {
  const m = useT().vesting;
  const locale = dateLocale(useLocale());
  // The share by the contract's rule on a total of a million when no amount is typed yet
  const sample: Terms = { total: total ?? 1_000_000n, released: 0n, start, cliff, duration, revoked_at: null };
  const date = (at: bigint) => (approximate ? m.about(formatDateTime(at, locale)) : formatDateTime(at, locale));
  const vested = (at: bigint) => {
    const v = vestedAt(sample, at);
    const pct = m.percent(percentText(v, sample.total));
    return total !== null && meta ? `${formatUnits(v, meta.decimals)} ${meta.name} · ${pct}` : pct;
  };
  const already = !approximate && start < now ? vestedAt(sample, now) : 0n;
  return <div className="vesting-preview">
    <h3 className="sub-title">{m.preview}</h3>
    <VestingChart start={start} cliff={cliff} duration={duration} now={approximate ? null : now}
      text={{
        label: m.chartLabel, vested: m.keys.vested, cliff: m.keys.cliff, now: m.keys.now, full: m.percent('100'),
        tick: (at) => formatTick(at, locale, duration), readout: (at) => m.readout(date(at), vested(at)),
      }} />
    <ul className="vesting-points">
      <li><span>{m.startsAt}</span><span className="vesting-point-value">{date(start)}</span></li>
      {previewOffsets(cliff, duration).map((offset) => <li key={offset.toString()}>
        <span>{offset === duration ? m.atEnd(durationText(m, offset)) : offset === cliff ? m.atCliff(durationText(m, offset)) : m.after(durationText(m, offset))}</span>
        <span className="vesting-point-value"><span className="mono">{vested(start + offset)}</span><span className="muted">{date(start + offset)}</span></span>
      </li>)}
    </ul>
    {!approximate && start < now && <p className="field-hint">{total !== null && meta && already > 0n
      ? m.startPastAmount(`${formatUnits(already, meta.decimals)} ${meta.name}`) : m.startPast}</p>}
  </div>;
}
