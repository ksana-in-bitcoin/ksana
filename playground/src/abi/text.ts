/**
 * Values as people type and read them: the drafts behind the generated forms (one text field, checkbox or list per type) and
 * the text a decoded value is shown as.
 */
import { type BtcNetwork, accountToAddress, fromHex, parseAccount, toHex } from '@ksana/sdk';
import { type Abi, type AbiType, type Value, intRange, isEnumValue } from './types';

export class InputError extends Error {
  constructor(readonly path: string, message: string) {
    super(path ? `${path}: ${message}` : message);
    this.name = 'InputError';
  }
}

/**
 * A form's state for one value: `text` for integers, text, bytes and IDs; `on` for a bool or an Option that is Some; `items` for
 * an Option's inner value, a Vec's or a tuple's items, a struct's fields and the selected enum variant's values; `variant` for
 * the selected variant.
 */
export interface Draft {
  text: string;
  on: boolean;
  items: Draft[];
  variant: number;
}

export interface TextContext {
  /** The BTC network of the node: accounts may be typed as Taproot addresses of it */
  network?: BtcNetwork | undefined;
  /** Whether an account is known to be a contract, which is written as its ID (`chain/contracts.ts`) */
  contract?: ((account: Uint8Array) => boolean) | undefined;
}

const ZERO32 = new Uint8Array(32);

export function emptyDraft(abi: Abi, type: AbiType): Draft {
  const d: Draft = { text: '', on: false, items: [], variant: 0 };
  switch (type.kind) {
    case 'option':
      d.items = [emptyDraft(abi, type.inner)];
      break;
    case 'tuple':
      d.items = type.items.map((t) => emptyDraft(abi, t));
      break;
    case 'struct':
      d.items = (abi.structs.get(type.name)?.fields ?? []).map((f) => emptyDraft(abi, f.type));
      break;
    case 'enum':
      d.items = (abi.enums.get(type.name)?.variants[0]?.fields ?? []).map((t) => emptyDraft(abi, t));
      break;
    default:
      break;
  }
  return d;
}

/** A draft holding `value` (to prefill a form) */
export function draftOf(abi: Abi, type: AbiType, value: Value): Draft {
  const d = emptyDraft(abi, type);
  switch (type.kind) {
    case 'int':
      d.text = typeof value === 'bigint' ? value.toString() : '';
      break;
    case 'bool':
      d.on = value === true;
      break;
    case 'string':
      d.text = typeof value === 'string' ? value : '';
      break;
    case 'bytes':
    case 'fixed':
    case 'id':
      // KSA's asset ID as the field's KSA button writes it, a token ID as `#n` when it is the n-th token's
      d.text = !(value instanceof Uint8Array) ? ''
        : type.kind === 'id' && type.name === 'AssetId' && value.every((b) => b === 0) ? 'KSA'
          : type.kind === 'id' && type.name === 'TokenId' ? tokenIdText(value) : toHex(value);
      break;
    case 'option':
      d.on = value !== null;
      if (value !== null) d.items = [draftOf(abi, type.inner, value)];
      break;
    case 'vec':
      d.items = Array.isArray(value) ? value.map((v) => draftOf(abi, type.inner, v)) : [];
      break;
    case 'tuple':
      if (Array.isArray(value)) d.items = type.items.map((t, i) => draftOf(abi, t, value[i] ?? null));
      break;
    case 'struct': {
      const fields = abi.structs.get(type.name)?.fields ?? [];
      if (value && typeof value === 'object' && !Array.isArray(value) && !(value instanceof Uint8Array) && !isEnumValue(value)) {
        const record = value as { [field: string]: Value };
        d.items = fields.map((f) => (f.name in record ? draftOf(abi, f.type, record[f.name]!) : emptyDraft(abi, f.type)));
      }
      break;
    }
    case 'enum': {
      const def = abi.enums.get(type.name);
      if (def && value !== null && isEnumValue(value)) {
        const index = def.variants.findIndex((v) => v.name === value.variant);
        if (index >= 0) {
          d.variant = index;
          d.items = def.variants[index]!.fields.map((t, i) => draftOf(abi, t, value.values[i] ?? null));
        }
      }
      break;
    }
  }
  return d;
}

/** Selects another variant of an enum draft, with empty values */
export function withVariant(abi: Abi, type: AbiType, draft: Draft, variant: number): Draft {
  if (type.kind !== 'enum') return draft;
  const fields = abi.enums.get(type.name)?.variants[variant]?.fields ?? [];
  return { ...draft, variant, items: fields.map((t) => emptyDraft(abi, t)) };
}

export function parseInteger(text: string, name: Parameters<typeof intRange>[0]): bigint {
  const t = text.trim().replace(/[_,\s]/g, '');
  if (!/^-?[0-9]+$/.test(t)) throw new Error('enter a whole number');
  const v = BigInt(t);
  const { min, max } = intRange(name);
  if (v < min || v > max) throw new Error(`must be between ${min} and ${max}`);
  return v;
}

export function parseHex(text: string, len?: number): Uint8Array {
  let t = text.trim();
  if (t.startsWith('0x') || t.startsWith('0X')) t = t.slice(2);
  if (!/^(?:[0-9a-fA-F]{2})*$/.test(t)) throw new Error('enter hex digits (two per byte)');
  if (len !== undefined && t.length !== len * 2) throw new Error(`enter ${len * 2} hex digits`);
  return fromHex(t);
}

/** An asset ID: 64 hex digits, or KSA */
export function parseAssetId(text: string): Uint8Array {
  return /^\s*ksa\s*$/i.test(text) ? ZERO32.slice() : parseHex(text, 32);
}

const U128_LIMIT = 1n << 128n;

/** `ks721::id(n)`: the ID of the `n`-th token, `n` big-endian in the last 16 bytes (`docs/STANDARDS.md` §4.1) */
export function tokenIdOf(n: bigint): Uint8Array {
  if (n < 0n || n >= U128_LIMIT) throw new Error('a token number is below 2^128');
  const id = new Uint8Array(32);
  let rest = n;
  for (let i = 31; i >= 16 && rest > 0n; i--) {
    id[i] = Number(rest & 0xffn);
    rest >>= 8n;
  }
  return id;
}

/** `ks721::serial`: the `n` of an ID `ks721::id(n)`, whose first 16 bytes are zero; null for any other ID */
export function serialOf(id: Uint8Array): bigint | null {
  if (id.length !== 32 || id.subarray(0, 16).some((b) => b !== 0)) return null;
  let n = 0n;
  for (const b of id.subarray(16)) n = (n << 8n) | BigInt(b);
  return n;
}

/** A token ID as wallets show it: `#n` for the `n`-th token's, else its hex */
export function tokenIdText(id: Uint8Array): string {
  const n = serialOf(id);
  return n === null ? toHex(id) : `#${n}`;
}

/** A token ID typed as `#n` or `n` (the `n`-th token's), or as 64 hex digits */
export function parseTokenId(text: string): Uint8Array {
  const t = text.trim();
  const serial = /^#?\s*([0-9][0-9_,]*)$/.exec(t);
  if (serial && (t.startsWith('#') || t.length < 64)) {
    const n = BigInt(serial[1]!.replace(/[_,]/g, ''));
    if (n >= U128_LIMIT) throw new Error('a token number is below 2^128');
    return tokenIdOf(n);
  }
  try {
    return parseHex(t, 32);
  } catch {
    throw new Error('enter #n, or 64 hex digits');
  }
}

/** The value a draft stands for; throws {@link InputError} naming the field */
export function valueOf(abi: Abi, type: AbiType, draft: Draft, ctx: TextContext = {}, path = ''): Value {
  try {
    switch (type.kind) {
      case 'int':
        return parseInteger(draft.text, type.name);
      case 'bool':
        return draft.on;
      case 'string':
        return draft.text;
      case 'bytes':
        return parseHex(draft.text);
      case 'fixed':
        return parseHex(draft.text, type.len);
      case 'id':
        if (type.name === 'Account') return parseAccount(draft.text, ctx.network);
        if (type.name === 'AssetId') return parseAssetId(draft.text);
        if (type.name === 'TokenId') return parseTokenId(draft.text);
        return parseHex(draft.text, 32);
      default:
        break;
    }
  } catch (e) {
    if (e instanceof InputError) throw e;
    throw new InputError(path, e instanceof Error ? e.message : String(e));
  }
  switch (type.kind) {
    case 'option':
      return draft.on ? valueOf(abi, type.inner, draft.items[0] ?? emptyDraft(abi, type.inner), ctx, path) : null;
    case 'vec':
      return draft.items.map((d, i) => valueOf(abi, type.inner, d, ctx, `${path}[${i + 1}]`));
    case 'tuple':
      return type.items.map((t, i) => valueOf(abi, t, draft.items[i] ?? emptyDraft(abi, t), ctx, `${path}.${i + 1}`));
    case 'struct': {
      const out: { [field: string]: Value } = {};
      (abi.structs.get(type.name)?.fields ?? []).forEach((f, i) => {
        out[f.name] = valueOf(abi, f.type, draft.items[i] ?? emptyDraft(abi, f.type), ctx, path ? `${path}.${f.name}` : f.name);
      });
      return out;
    }
    case 'enum': {
      const variant = abi.enums.get(type.name)?.variants[draft.variant];
      if (!variant) throw new InputError(path, 'choose a variant');
      return {
        variant: variant.name,
        values: variant.fields.map((t, i) => valueOf(abi, t, draft.items[i] ?? emptyDraft(abi, t), ctx, `${path}.${variant.name}.${i + 1}`)),
      };
    }
    default:
      throw new InputError(path, 'unsupported type');
  }
}

/**
 * An account as text: its Taproot address on `network` when it is a key, else its hex. A contract has no address, but its ID can
 * be a valid key all the same: `contract` says it is one, written as its ID.
 */
export function accountText(account: Uint8Array, network?: BtcNetwork, contract = false): string {
  if (network && !contract) {
    try {
      return accountToAddress(account, network);
    } catch {
      /* a contract ID or another non-key value */
    }
  }
  return toHex(account);
}

/** A value as one line of text, as the ABI block would write it */
export function formatValue(abi: Abi, type: AbiType, value: Value, ctx: TextContext = {}): string {
  switch (type.kind) {
    case 'int':
      return typeof value === 'bigint' ? value.toString() : '?';
    case 'bool':
      return value === true ? 'true' : 'false';
    case 'string':
      return JSON.stringify(typeof value === 'string' ? value : '');
    case 'bytes':
    case 'fixed':
      return value instanceof Uint8Array ? (value.length ? `0x${toHex(value)}` : '0x') : '?';
    case 'id':
      if (!(value instanceof Uint8Array)) return '?';
      if (type.name === 'AssetId' && value.every((b) => b === 0)) return 'KSA';
      if (type.name === 'TokenId') return tokenIdText(value);
      return type.name === 'Account' ? accountText(value, ctx.network, ctx.contract?.(value)) : toHex(value);
    case 'option':
      return value === null ? 'None' : formatValue(abi, type.inner, value, ctx);
    case 'vec':
      return Array.isArray(value) ? `[${value.map((v) => formatValue(abi, type.inner, v, ctx)).join(', ')}]` : '?';
    case 'tuple':
      return Array.isArray(value) ? `(${type.items.map((t, i) => formatValue(abi, t, value[i] ?? null, ctx)).join(', ')})` : '?';
    case 'struct': {
      const def = abi.structs.get(type.name);
      if (!def || !value || typeof value !== 'object' || Array.isArray(value) || value instanceof Uint8Array || isEnumValue(value)) return '?';
      const record = value as { [field: string]: Value };
      return `{ ${def.fields.map((f) => `${f.name}: ${formatValue(abi, f.type, record[f.name] ?? null, ctx)}`).join(', ')} }`;
    }
    case 'enum': {
      const def = abi.enums.get(type.name);
      if (!def || value === null || !isEnumValue(value)) return '?';
      const variant = def.variants.find((v) => v.name === value.variant);
      if (!variant || variant.fields.length === 0) return value.variant;
      return `${value.variant}(${variant.fields.map((t, i) => formatValue(abi, t, value.values[i] ?? null, ctx)).join(', ')})`;
    }
  }
}
