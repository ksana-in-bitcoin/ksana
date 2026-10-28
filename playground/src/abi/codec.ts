/**
 * Borsh by ABI type: what a contract decodes with `ksana_contract_sdk::decode` and encodes with `sdk::ok` / `sdk::emit`.
 * Integers little-endian, `bool` one byte 0 / 1, `String` and `Vec` a u32 length and the items, `[u8; N]` and the IDs their bytes,
 * `Option` a tag 0 / 1, tuples and structs their items in order, enums a u8 tag (declaration order) and the variant's items.
 * Decoding is strict, like the contract SDK's: the input must be consumed exactly.
 */
import { type Abi, type AbiType, type EnumValue, type Value, INT_BITS, intRange, isEnumValue, typeName } from './types';

export class CodecError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'CodecError';
  }
}

const utf8 = new TextEncoder();
// `ignoreBOM`: a leading U+FEFF is part of the text, as in Rust's `str::from_utf8`; the default would drop it
const utf8Strict = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

class Writer {
  private buf = new Uint8Array(64);
  private len = 0;

  private room(n: number) {
    if (this.len + n <= this.buf.length) return;
    let size = this.buf.length * 2;
    while (size < this.len + n) size *= 2;
    const next = new Uint8Array(size);
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }

  bytes(b: Uint8Array) {
    this.room(b.length);
    this.buf.set(b, this.len);
    this.len += b.length;
  }

  u8(v: number) {
    this.room(1);
    this.buf[this.len++] = v;
  }

  int(v: bigint, bits: number) {
    const n = bits / 8;
    this.room(n);
    let x = BigInt.asUintN(bits, v);
    for (let i = 0; i < n; i++) {
      this.buf[this.len++] = Number(x & 0xffn);
      x >>= 8n;
    }
  }

  done(): Uint8Array {
    return this.buf.slice(0, this.len);
  }
}

class Reader {
  pos = 0;
  constructor(readonly buf: Uint8Array) {}

  take(n: number): Uint8Array {
    if (n < 0 || this.pos + n > this.buf.length) throw new CodecError('unexpected end of data');
    const out = this.buf.subarray(this.pos, this.pos + n);
    this.pos += n;
    return out;
  }

  u8(): number {
    return this.take(1)[0]!;
  }

  uint(bits: number): bigint {
    const b = this.take(bits / 8);
    let x = 0n;
    for (let i = b.length - 1; i >= 0; i--) x = (x << 8n) | BigInt(b[i]!);
    return x;
  }

  get remaining(): number {
    return this.buf.length - this.pos;
  }
}

function where(path: string): string {
  return path ? ` at ${path}` : '';
}

/**
 * Whether the Rust type of `type` takes no memory: `()`, a tuple or a struct of such types, `[u8; 0]`, an enum with one variant of
 * such fields. The contract SDK refuses a list of them whatever its length, as Borsh does (`Decode::decode_vec`).
 */
function zeroSized(abi: Abi, type: AbiType, depth = 0): boolean {
  if (depth > 64) return false;
  switch (type.kind) {
    case 'fixed':
      return type.len === 0;
    case 'tuple':
      return type.items.every((t) => zeroSized(abi, t, depth + 1));
    case 'struct':
      return abi.structs.get(type.name)?.fields.every((f) => zeroSized(abi, f.type, depth + 1)) ?? false;
    case 'enum': {
      const variants = abi.enums.get(type.name)?.variants;
      if (variants === undefined) return false;
      return variants.length === 0 || (variants.length === 1 && variants[0]!.fields.every((t) => zeroSized(abi, t, depth + 1)));
    }
    default:
      return false;
  }
}

function write(abi: Abi, w: Writer, type: AbiType, value: Value, path: string): void {
  switch (type.kind) {
    case 'int': {
      if (typeof value !== 'bigint') throw new CodecError(`expected an integer${where(path)}`);
      const { min, max } = intRange(type.name);
      if (value < min || value > max) throw new CodecError(`${value} is out of range for ${type.name}${where(path)}`);
      w.int(value, INT_BITS[type.name]);
      return;
    }
    case 'bool':
      if (typeof value !== 'boolean') throw new CodecError(`expected true or false${where(path)}`);
      w.u8(value ? 1 : 0);
      return;
    case 'string': {
      if (typeof value !== 'string') throw new CodecError(`expected text${where(path)}`);
      const b = utf8.encode(value);
      w.int(BigInt(b.length), 32);
      w.bytes(b);
      return;
    }
    case 'bytes':
      if (!(value instanceof Uint8Array)) throw new CodecError(`expected bytes${where(path)}`);
      w.int(BigInt(value.length), 32);
      w.bytes(value);
      return;
    case 'fixed':
    case 'id': {
      const len = type.kind === 'fixed' ? type.len : 32;
      if (!(value instanceof Uint8Array) || value.length !== len) throw new CodecError(`expected ${len} bytes${where(path)}`);
      w.bytes(value);
      return;
    }
    case 'option':
      if (value === null) {
        w.u8(0);
      } else {
        w.u8(1);
        write(abi, w, type.inner, value, path);
      }
      return;
    case 'vec':
    case 'tuple': {
      if (!Array.isArray(value)) throw new CodecError(`expected a list${where(path)}`);
      if (type.kind === 'vec') {
        w.int(BigInt(value.length), 32);
        value.forEach((v, i) => write(abi, w, type.inner, v, `${path}[${i}]`));
      } else {
        if (value.length !== type.items.length) throw new CodecError(`expected ${type.items.length} items${where(path)}`);
        type.items.forEach((t, i) => write(abi, w, t, value[i]!, `${path}.${i}`));
      }
      return;
    }
    case 'struct': {
      const def = abi.structs.get(type.name);
      if (!def) throw new CodecError(`unknown struct ${type.name}`);
      if (typeof value !== 'object' || value === null || Array.isArray(value) || value instanceof Uint8Array || isEnumValue(value)) {
        throw new CodecError(`expected ${type.name}${where(path)}`);
      }
      const record = value as { [field: string]: Value };
      for (const f of def.fields) {
        if (!(f.name in record)) throw new CodecError(`missing ${f.name}${where(path)}`);
        write(abi, w, f.type, record[f.name]!, path ? `${path}.${f.name}` : f.name);
      }
      return;
    }
    case 'enum': {
      const def = abi.enums.get(type.name);
      if (!def) throw new CodecError(`unknown enum ${type.name}`);
      if (!isEnumValue(value)) throw new CodecError(`expected a variant of ${type.name}${where(path)}`);
      const tag = def.variants.findIndex((v) => v.name === value.variant);
      const variant = def.variants[tag];
      if (!variant) throw new CodecError(`${type.name} has no variant ${value.variant}${where(path)}`);
      if (value.values.length !== variant.fields.length) throw new CodecError(`${value.variant} takes ${variant.fields.length} values${where(path)}`);
      w.u8(tag);
      variant.fields.forEach((t, i) => write(abi, w, t, value.values[i]!, `${path}.${value.variant}.${i}`));
      return;
    }
  }
}

function read(abi: Abi, r: Reader, type: AbiType, depth: number): Value {
  if (depth > 64) throw new CodecError('value nested too deeply');
  switch (type.kind) {
    case 'int': {
      const bits = INT_BITS[type.name];
      const raw = r.uint(bits);
      return type.name.startsWith('i') ? BigInt.asIntN(bits, raw) : raw;
    }
    case 'bool': {
      const b = r.u8();
      if (b > 1) throw new CodecError(`invalid bool ${b}`);
      return b === 1;
    }
    case 'string': {
      const len = Number(r.uint(32));
      try {
        return utf8Strict.decode(r.take(len));
      } catch (e) {
        if (e instanceof CodecError) throw e;
        throw new CodecError('invalid UTF-8');
      }
    }
    case 'bytes':
      return r.take(Number(r.uint(32))).slice();
    case 'fixed':
      return r.take(type.len).slice();
    case 'id':
      return r.take(32).slice();
    case 'option': {
      const tag = r.u8();
      if (tag === 0) return null;
      if (tag !== 1) throw new CodecError(`invalid option tag ${tag}`);
      return read(abi, r, type.inner, depth + 1);
    }
    case 'vec': {
      const len = Number(r.uint(32));
      // every item takes at least one byte (a list of a zero-sized type is refused, as Borsh does)
      if (len > r.remaining || zeroSized(abi, type.inner)) throw new CodecError('invalid list length');
      const out: Value[] = [];
      for (let i = 0; i < len; i++) out.push(read(abi, r, type.inner, depth + 1));
      return out;
    }
    case 'tuple':
      return type.items.map((t) => read(abi, r, t, depth + 1));
    case 'struct': {
      const def = abi.structs.get(type.name);
      if (!def) throw new CodecError(`unknown struct ${type.name}`);
      const out: { [field: string]: Value } = {};
      for (const f of def.fields) out[f.name] = read(abi, r, f.type, depth + 1);
      return out;
    }
    case 'enum': {
      const def = abi.enums.get(type.name);
      if (!def) throw new CodecError(`unknown enum ${type.name}`);
      const tag = r.u8();
      const variant = def.variants[tag];
      if (!variant) throw new CodecError(`invalid ${type.name} tag ${tag}`);
      return { variant: variant.name, values: variant.fields.map((t) => read(abi, r, t, depth + 1)) } satisfies EnumValue;
    }
  }
}

/** Encodes one value */
export function encodeValue(abi: Abi, type: AbiType, value: Value): Uint8Array {
  const w = new Writer();
  write(abi, w, type, value, '');
  return w.done();
}

/** Encodes a method's arguments: the tuple of its parameters (a single parameter is itself, none is empty bytes) */
export function encodeArgs(abi: Abi, types: AbiType[], values: Value[]): Uint8Array {
  if (values.length !== types.length) throw new CodecError(`expected ${types.length} arguments`);
  const w = new Writer();
  types.forEach((t, i) => write(abi, w, t, values[i]!, `#${i + 1}`));
  return w.done();
}

/** Decodes a value that must take all of `bytes` */
export function decodeValue(abi: Abi, type: AbiType, bytes: Uint8Array): Value {
  const r = new Reader(bytes);
  const value = read(abi, r, type, 0);
  if (r.remaining !== 0) throw new CodecError(`${r.remaining} bytes left after ${typeName(type)}`);
  return value;
}

/** Decodes a method's return value; `()` when it returns nothing */
export function decodeReturn(abi: Abi, type: AbiType | null, bytes: Uint8Array): Value {
  return decodeValue(abi, type ?? { kind: 'tuple', items: [] }, bytes);
}

/**
 * Decodes an event: a Borsh tuple whose first field is the event's name. Null when the data does not start with the name of an
 * event the ABI declares, or does not match its fields.
 */
export function decodeEvent(abi: Abi, data: Uint8Array): { name: string; fields: { name: string; value: Value }[] } | null {
  try {
    const r = new Reader(data);
    const name = read(abi, r, { kind: 'string' }, 0) as string;
    const def = abi.events.find((e) => e.name === name);
    if (!def) return null;
    const fields = def.fields.map((f) => ({ name: f.name, value: read(abi, r, f.type, 0) }));
    if (r.remaining !== 0) return null;
    return { name, fields };
  } catch {
    return null;
  }
}
