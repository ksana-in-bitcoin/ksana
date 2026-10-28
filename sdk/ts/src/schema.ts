/**
 * Type definitions: one definition gives both the Borsh codec (via the `borsh` package) and the JSON representation of
 * D-007.
 *
 * The `borsh` package itself does not check integer ranges or that the input is fully consumed, and its UTF-8 decoding is
 * not strict, so a layer is added around it here:
 *
 * - validate field by field before encoding (integer ranges, lengths of fixed-size byte strings, strings without lone
 *   surrogates);
 * - check the length limit before decoding (SPEC §16), validate field by field after decoding, then re-encode and require
 *   the result to be **byte-for-byte identical** to the input: this rules out both trailing bytes and every non-canonical
 *   encoding (invalid UTF-8 etc.), matching the Rust side's property that `decode(b) == Ok(v)` implies `encode(v) == b`.
 *
 * JSON (D-007): byte strings are lowercase hex (upper case is accepted on input too, like Rust's `hex` crate), `u64` /
 * `u128` are decimal strings (only the canonical notation is accepted), `u8` / `u32` are numbers, enums use serde's
 * externally tagged form; unknown fields are rejected and all fields are required.
 */

import { deserialize, serialize } from 'borsh';

import { equalBytes, fromHex, toHex } from './bytes.js';

/** The schema type of the `borsh` package. */
export type BorshSchema = Parameters<typeof serialize>[0];

/** A JSON value. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** Codec error (including JSON conversion errors). */
export class CodecError extends Error {
  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = 'CodecError';
  }
}

/** Definition of one type. `path` is used in error messages to name the failing field. */
export interface TypeDef<T> {
  /** Schema for the `borsh` package. */
  readonly schema: BorshSchema;
  /** Validate and convert to the value used by the `borsh` package. */
  toBorsh(value: T, path: string): unknown;
  /** Convert (and validate) the value decoded by the `borsh` package. */
  fromBorsh(raw: unknown, path: string): T;
  /** Convert to JSON (D-007). */
  toJson(value: T, path: string): Json;
  /** Convert from JSON (D-007), rejecting unknown fields and non-canonical notation. */
  fromJson(json: unknown, path: string): T;
  /**
   * An encoding that the `borsh` package's schema cannot describe (the compact headers in a payload, D-075): when given,
   * `encodeWith` / `decodeWith` call it directly (`decodeWith` still checks the length limit first and then requires the
   * re-encoding to be byte-for-byte identical), `schema` / `toBorsh` / `fromBorsh` are not used for encoding or decoding,
   * and such a type cannot be nested inside another type.
   */
  readonly codec?: {
    /** Validate and encode. */
    encode(value: T, path: string): Uint8Array;
    /** Decode; the input must be fully consumed. Throws `CodecError` on failure. */
    decode(bytes: Uint8Array): T;
  };
}

/** A member of a composite type cannot be a type with its own codec (its `schema` does not describe the real encoding). */
function nested<T>(def: TypeDef<T>): TypeDef<T> {
  if (def.codec !== undefined) throw new TypeError('a type with its own codec (such as the payload) cannot be nested inside another type');
  return def;
}

function fail(path: string, message: string): never {
  throw new CodecError(`${path}: ${message}`);
}

function describe(value: unknown): string {
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'array';
  if (value instanceof Uint8Array) return `Uint8Array(${value.length})`;
  if (typeof value === 'bigint') return `${value}n`;
  if (typeof value === 'string') return JSON.stringify(value.length > 40 ? `${value.slice(0, 40)}…` : value);
  if (typeof value === 'object') return 'object';
  return String(value);
}

function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array);
}

function hasOwn(object: object, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(object, key);
}

// ---------------------------------------------------------------------------------------------
// Integers

function smallUint(bits: 8 | 32): TypeDef<number> {
  const max = bits === 8 ? 0xff : 0xffff_ffff;
  const name = `u${bits}`;
  const check = (value: unknown, path: string): number => {
    if (typeof value !== 'number' || !Number.isInteger(value) || value < 0 || value > max) {
      fail(path, `expected ${name} (an integer from 0 to ${max}), got ${describe(value)}`);
    }
    return value;
  };
  return {
    schema: name,
    toBorsh: check,
    fromBorsh: check,
    toJson: check,
    fromJson: check,
  };
}

/** A canonical decimal integer: digits only, no sign, no redundant leading zeros (same as `parse_dec` on the Rust side). */
export function parseCanonicalDecimal(text: string, path = '$'): bigint {
  if (!/^(0|[1-9][0-9]*)$/.test(text)) {
    fail(path, `non-canonical decimal integer: ${describe(text)}`);
  }
  return BigInt(text);
}

function bigUint(bits: 64 | 128): TypeDef<bigint> {
  const max = (1n << BigInt(bits)) - 1n;
  const name = `u${bits}`;
  const check = (value: unknown, path: string): bigint => {
    if (typeof value !== 'bigint' || value < 0n || value > max) {
      fail(path, `expected ${name} (a bigint from 0 to ${max}), got ${describe(value)}`);
    }
    return value;
  };
  return {
    schema: name,
    toBorsh: check,
    fromBorsh: check,
    toJson: (value, path) => check(value, path).toString(),
    fromJson: (json, path) => {
      if (typeof json !== 'string') fail(path, `expected a decimal string, got ${describe(json)}`);
      const value = parseCanonicalDecimal(json, path);
      if (value > max) fail(path, `integer out of ${name} range: ${json}`);
      return value;
    },
  };
}

/** `u8`. */
export const u8 = smallUint(8);
/** `u32`. */
export const u32 = smallUint(32);
/** `u64`. */
export const u64 = bigUint(64);
/** `u128`. */
export const u128 = bigUint(128);

/** `bool`: one byte, 0 or 1 (`decodeWith` rejects any other byte, as its re-encoding differs). */
export const bool: TypeDef<boolean> = (() => {
  const check = (value: unknown, path: string): boolean => {
    if (typeof value !== 'boolean') fail(path, `expected a boolean, got ${describe(value)}`);
    return value;
  };
  return {
    schema: 'bool',
    toBorsh: check,
    fromBorsh: check,
    toJson: check,
    fromJson: check,
  };
})();

// ---------------------------------------------------------------------------------------------
// Byte strings and strings

/** The `borsh` package decodes a u8 array as `number[]`. */
function bytesFromBorsh(raw: unknown, path: string, length?: number): Uint8Array {
  if (!Array.isArray(raw)) fail(path, 'decoded value is not a byte array');
  if (length !== undefined && raw.length !== length) fail(path, `expected ${length} bytes`);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) {
    const byte: unknown = raw[i];
    if (typeof byte !== 'number' || !Number.isInteger(byte) || byte < 0 || byte > 0xff) {
      fail(path, 'decoded value is not a byte array');
    }
    out[i] = byte;
  }
  return out;
}

/**
 * A byte string for the `borsh` package: copied into a new `ArrayBuffer` of exactly the right size (it writes the whole
 * buffer instead of encoding byte by byte). `value.slice().buffer` cannot be used: `slice` of a Node.js `Buffer` is a
 * view sharing memory, and `.buffer` is the whole memory pool.
 */
function bytesToBorsh(value: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(value.length);
  copy.set(value);
  return copy.buffer;
}

/** `[u8; N]`. */
export function fixedBytes(length: number): TypeDef<Uint8Array> {
  const check = (value: unknown, path: string): Uint8Array => {
    if (!(value instanceof Uint8Array) || value.length !== length) {
      fail(path, `expected a Uint8Array of ${length} bytes, got ${describe(value)}`);
    }
    return value;
  };
  return {
    schema: { array: { type: 'u8', len: length } },
    toBorsh: (value, path) => bytesToBorsh(check(value, path)),
    fromBorsh: (raw, path) => bytesFromBorsh(raw, path, length),
    toJson: (value, path) => toHex(check(value, path)),
    fromJson: (json, path) => {
      if (typeof json !== 'string') fail(path, `expected a hex string, got ${describe(json)}`);
      try {
        return fromHex(json, length);
      } catch (e) {
        return fail(path, errorMessage(e));
      }
    },
  };
}

/** `Vec<u8>`. */
export const vecU8: TypeDef<Uint8Array> = (() => {
  const check = (value: unknown, path: string): Uint8Array => {
    if (!(value instanceof Uint8Array) || value.length > 0xffff_ffff) {
      fail(path, `expected a Uint8Array, got ${describe(value)}`);
    }
    return value;
  };
  return {
    schema: { array: { type: 'u8' } },
    toBorsh: (value, path) => bytesToBorsh(check(value, path)),
    fromBorsh: (raw, path) => bytesFromBorsh(raw, path),
    toJson: (value, path) => toHex(check(value, path)),
    fromJson: (json, path) => {
      if (typeof json !== 'string') fail(path, `expected a hex string, got ${describe(json)}`);
      try {
        return fromHex(json);
      } catch (e) {
        return fail(path, errorMessage(e));
      }
    },
  };
})();

/** Whether the string is a valid sequence of Unicode scalar values (no lone UTF-16 surrogates), i.e. can be encoded as UTF-8 losslessly. */
export function isWellFormedString(text: string): boolean {
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i);
    if (unit >= 0xd800 && unit <= 0xdbff) {
      const next = text.charCodeAt(i + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) return false;
      i++;
    } else if (unit >= 0xdc00 && unit <= 0xdfff) {
      return false;
    }
  }
  return true;
}

/** `String` (UTF-8). */
export const str: TypeDef<string> = (() => {
  const check = (value: unknown, path: string): string => {
    if (typeof value !== 'string') fail(path, `expected a string, got ${describe(value)}`);
    if (!isWellFormedString(value)) fail(path, 'string contains a lone UTF-16 surrogate and cannot be encoded as UTF-8');
    return value;
  };
  return { schema: 'string', toBorsh: check, fromBorsh: check, toJson: check, fromJson: check };
})();

// ---------------------------------------------------------------------------------------------
// Composite types

/** `Vec<T>`. */
export function vec<T>(item: TypeDef<T>): TypeDef<T[]> {
  nested(item);
  const check = (value: unknown, path: string): unknown[] => {
    if (!Array.isArray(value)) fail(path, `expected an array, got ${describe(value)}`);
    if (value.length > 0xffff_ffff) fail(path, 'array too long');
    return value;
  };
  return {
    schema: { array: { type: item.schema } },
    toBorsh: (value, path) => check(value, path).map((x, i) => item.toBorsh(x as T, `${path}[${i}]`)),
    fromBorsh: (raw, path) => check(raw, path).map((x, i) => item.fromBorsh(x, `${path}[${i}]`)),
    toJson: (value, path) => check(value, path).map((x, i) => item.toJson(x as T, `${path}[${i}]`)),
    fromJson: (json, path) => check(json, path).map((x, i) => item.fromJson(x, `${path}[${i}]`)),
  };
}

/** `Option<T>`, `T | null` in TS. */
export function option<T>(item: TypeDef<T>): TypeDef<T | null> {
  nested(item);
  return {
    schema: { option: item.schema },
    toBorsh: (value, path) => (value === null ? null : item.toBorsh(value, path)),
    fromBorsh: (raw, path) => (raw === null ? null : item.fromBorsh(raw, path)),
    toJson: (value, path) => (value === null ? null : item.toJson(value, path)),
    fromJson: (json, path) => (json === null ? null : item.fromJson(json, path)),
  };
}

/** Struct fields: `[field name, type]`, in declaration order (which is the encoding order). */
export type Fields = ReadonlyArray<readonly [string, TypeDef<unknown>]>;

function structSchema(fields: Fields): BorshSchema {
  const struct: Record<string, BorshSchema> = {};
  for (const [name, def] of fields) struct[name] = nested(def).schema;
  return { struct };
}

function fieldsToBorsh(fields: Fields, value: unknown, path: string): Record<string, unknown> {
  if (!isPlainObject(value)) fail(path, `expected an object, got ${describe(value)}`);
  const out: Record<string, unknown> = {};
  for (const [name, def] of fields) out[name] = def.toBorsh(value[name], `${path}.${name}`);
  return out;
}

function fieldsFromBorsh(fields: Fields, raw: unknown, path: string): Record<string, unknown> {
  if (!isPlainObject(raw)) fail(path, 'decoded value is not an object');
  const out: Record<string, unknown> = {};
  for (const [name, def] of fields) out[name] = def.fromBorsh(raw[name], `${path}.${name}`);
  return out;
}

function fieldsToJson(fields: Fields, value: unknown, path: string): { [key: string]: Json } {
  if (!isPlainObject(value)) fail(path, `expected an object, got ${describe(value)}`);
  const out: { [key: string]: Json } = {};
  for (const [name, def] of fields) out[name] = def.toJson(value[name], `${path}.${name}`);
  return out;
}

function fieldsFromJson(fields: Fields, json: unknown, path: string): Record<string, unknown> {
  if (!isPlainObject(json)) fail(path, `expected a JSON object, got ${describe(json)}`);
  const known = new Set(fields.map(([name]) => name));
  for (const key of Object.keys(json)) {
    if (!known.has(key)) fail(path, `unknown field ${JSON.stringify(key)}`);
  }
  const out: Record<string, unknown> = {};
  for (const [name, def] of fields) {
    if (!hasOwn(json, name)) fail(path, `missing field ${JSON.stringify(name)}`);
    out[name] = def.fromJson(json[name], `${path}.${name}`);
  }
  return out;
}

/** Struct. */
export function struct<T extends object>(fields: Fields): TypeDef<T> {
  return {
    schema: structSchema(fields),
    toBorsh: (value, path) => fieldsToBorsh(fields, value, path),
    fromBorsh: (raw, path) => fieldsFromBorsh(fields, raw, path) as T,
    toJson: (value, path) => fieldsToJson(fields, value, path),
    fromJson: (json, path) => fieldsFromJson(fields, json, path) as T,
  };
}

/** Enum variant: a unit variant, a tuple variant (its content goes in the field `field`) or a struct variant. */
export type Variant =
  | { readonly name: string; readonly shape: 'unit' }
    | { readonly name: string; readonly shape: 'tuple'; readonly field: string; readonly def: TypeDef<unknown> }
  | { readonly name: string; readonly shape: 'struct'; readonly fields: Fields };

/** Unit variant. */
export const unitVariant = (name: string): Variant => ({ name, shape: 'unit' });
/** Tuple variant (a single field). */
export const tupleVariant = (name: string, field: string, def: TypeDef<unknown>): Variant => ({
  name,
  shape: 'tuple',
  field,
  def,
});
/** Struct variant. */
export const structVariant = (name: string, fields: Fields): Variant => ({ name, shape: 'struct', fields });

/**
 * Enum: Borsh writes the 1-byte variant index first (in declaration order); in TS it is `{ kind: variant name, ... }`,
 * and in JSON it is serde's externally tagged form (a string for a unit variant, otherwise an object with a single key).
 */
export function enumType<T extends { kind: string }>(variants: readonly Variant[]): TypeDef<T> {
  const byName = new Map(variants.map((v) => [v.name, v]));
  const inner = (v: Variant): BorshSchema => {
    switch (v.shape) {
      case 'unit':
        return { struct: {} };
      case 'tuple':
        return nested(v.def).schema;
      case 'struct':
        return structSchema(v.fields);
    }
  };
  const schema: BorshSchema = { enum: variants.map((v) => ({ struct: { [v.name]: inner(v) } })) };
  const variantOf = (value: unknown, path: string): Variant => {
    if (!isPlainObject(value) || typeof value.kind !== 'string') {
      fail(path, `expected an object with kind, got ${describe(value)}`);
    }
    const variant = byName.get(value.kind);
    if (variant === undefined) fail(path, `unknown variant ${JSON.stringify(value.kind)}`);
    return variant;
  };
  return {
    schema,
    toBorsh(value, path) {
      const variant = variantOf(value, path);
      const v = value as unknown as Record<string, unknown>;
      switch (variant.shape) {
        case 'unit':
          return { [variant.name]: {} };
        case 'tuple':
          return { [variant.name]: variant.def.toBorsh(v[variant.field], `${path}.${variant.field}`) };
        case 'struct':
          return { [variant.name]: fieldsToBorsh(variant.fields, v, path) };
      }
    },
    fromBorsh(raw, path) {
      if (!isPlainObject(raw)) fail(path, 'decoded value is not an enum');
      const keys = Object.keys(raw);
      const name = keys[0];
      const variant = keys.length === 1 && name !== undefined ? byName.get(name) : undefined;
      if (variant === undefined || name === undefined) fail(path, 'decoded value is not an enum');
      switch (variant.shape) {
        case 'unit':
          return { kind: name } as T;
        case 'tuple':
          return { kind: name, [variant.field]: variant.def.fromBorsh(raw[name], `${path}.${variant.field}`) } as T;
        case 'struct':
          return { kind: name, ...fieldsFromBorsh(variant.fields, raw[name], path) } as T;
      }
    },
    toJson(value, path) {
      const variant = variantOf(value, path);
      const v = value as unknown as Record<string, unknown>;
      switch (variant.shape) {
        case 'unit':
          return variant.name;
        case 'tuple':
          return { [variant.name]: variant.def.toJson(v[variant.field], `${path}.${variant.field}`) };
        case 'struct':
          return { [variant.name]: fieldsToJson(variant.fields, v, path) };
      }
    },
    fromJson(json, path) {
      if (typeof json === 'string') {
        const variant = byName.get(json);
        if (variant?.shape !== 'unit') fail(path, `unknown unit variant ${describe(json)}`);
        return { kind: json } as T;
      }
      if (!isPlainObject(json)) fail(path, `expected an enum (a string or an object with a single key), got ${describe(json)}`);
      const keys = Object.keys(json);
      const name = keys[0];
      if (keys.length !== 1 || name === undefined) fail(path, 'an enum object must have exactly one key (the variant name)');
      const variant = byName.get(name);
      if (variant === undefined) fail(path, `unknown variant ${JSON.stringify(name)}`);
      const body = json[name];
      const at = `${path}.${name}`;
      switch (variant.shape) {
        case 'unit':
          if (body !== null) fail(at, 'the value of a unit variant can only be null');
          return { kind: name } as T;
        case 'tuple':
          return { kind: name, [variant.field]: variant.def.fromJson(body, at) } as T;
        case 'struct':
          return { kind: name, ...fieldsFromJson(variant.fields, body, at) } as T;
      }
    },
  };
}

// ---------------------------------------------------------------------------------------------
// Codec entry points

/** Borsh encoding (validates field by field first). */
export function encodeWith<T>(def: TypeDef<T>, value: T): Uint8Array {
  if (def.codec !== undefined) return def.codec.encode(value, '$');
  const raw = def.toBorsh(value, '$');
  try {
    return serialize(def.schema, raw, true);
  } catch (e) {
    throw new CodecError(`Borsh encode failed: ${errorMessage(e)}`, { cause: e });
  }
}

/**
 * Borsh decoding: first checks `bytes.length <= maxLength` (anything longer is rejected without attempting to decode),
 * then decodes; the input must be fully consumed and canonically encoded (byte-for-byte identical after re-encoding).
 */
export function decodeWith<T>(def: TypeDef<T>, bytes: Uint8Array, maxLength: number): T {
  if (!(bytes instanceof Uint8Array)) {
    throw new TypeError('the input to decode must be a Uint8Array');
  }
  if (bytes.length > maxLength) {
    throw new CodecError(`encoded length ${bytes.length} exceeds the limit ${maxLength}`);
  }
  let value: T;
  if (def.codec !== undefined) {
    value = def.codec.decode(bytes);
  } else {
    let raw: unknown;
    try {
      raw = deserialize(def.schema, bytes, true);
    } catch (e) {
      throw new CodecError(`Borsh decode failed: ${errorMessage(e)}`, { cause: e });
    }
    value = def.fromBorsh(raw, '$');
  }
  if (!equalBytes(encodeWith(def, value), bytes)) {
    throw new CodecError('Borsh decode failed: the input has trailing bytes or is not canonically encoded (e.g. invalid UTF-8)');
  }
  return value;
}
