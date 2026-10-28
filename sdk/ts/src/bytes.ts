/**
 * Byte utilities: hex, concatenation, comparison, little-endian integers, CompactSize, UTF-8 and random bytes.
 *
 * Only Web standard APIs are used (`TextEncoder`, `crypto.getRandomValues`), so this works in Node.js (≥ 20) and
 * browsers alike.
 */

const HEX_DIGITS = '0123456789abcdef';

/** Bytes → lowercase hex (D-007). */
export function toHex(bytes: Uint8Array): string {
  let out = '';
  for (const byte of bytes) {
    out += HEX_DIGITS[byte >> 4]! + HEX_DIGITS[byte & 0x0f]!;
  }
  return out;
}

/**
 * Hex → bytes. Accepts upper and lower case (like Rust's `hex` crate), rejects a `0x` prefix and whitespace;
 * when `length` is given, exactly that many bytes are required.
 */
export function fromHex(text: string, length?: number, what = 'hex'): Uint8Array {
  if (typeof text !== 'string') {
    throw new TypeError(`${what} must be a string`);
  }
  if (text.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(text)) {
    throw new Error(`${what} is not a valid hex string`);
  }
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(text.slice(i * 2, i * 2 + 2), 16);
  }
  if (length !== undefined && out.length !== length) {
    throw new Error(`${what} must be ${length} bytes, got ${out.length} bytes`);
  }
  return out;
}

/** Concatenate byte strings. */
export function concatBytes(...parts: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const part of parts) total += part.length;
  const out = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

/** Whether two byte strings are identical. */
export function equalBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

/**
 * Reverse the byte order (returns a copy). The internal byte order of a BTC transaction ID is the reverse of the byte
 * order shown by RPC (D-001).
 */
export function reverseBytes(bytes: Uint8Array): Uint8Array {
  return Uint8Array.from(bytes).reverse();
}

/** Whether `value` is a `Uint8Array` (including subclasses). */
export function isBytes(value: unknown): value is Uint8Array {
  return value instanceof Uint8Array;
}

/** Require `value` to be a byte string, of the given length when one is given. */
export function assertBytes(value: unknown, length?: number, what = 'bytes'): Uint8Array {
  if (!isBytes(value)) {
    throw new TypeError(`${what} must be a Uint8Array`);
  }
  if (length !== undefined && value.length !== length) {
    throw new Error(`${what} must be ${length} bytes, got ${value.length} bytes`);
  }
  return value;
}

/** UTF-8 encoding. */
export function utf8(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}

/** Random bytes from `crypto.getRandomValues`. */
export function randomBytes(length: number): Uint8Array {
  const out = new Uint8Array(length);
  globalThis.crypto.getRandomValues(out);
  return out;
}

/** Little-endian u32. */
export function u32le(value: number): Uint8Array {
  if (!Number.isInteger(value) || value < 0 || value > 0xffff_ffff) {
    throw new RangeError(`u32 out of range: ${value}`);
  }
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, value, true);
  return out;
}

/** Little-endian i32 (transaction versions are serialized as signed integers). */
export function i32le(value: number): Uint8Array {
  if (!Number.isInteger(value) || value < -0x8000_0000 || value > 0x7fff_ffff) {
    throw new RangeError(`i32 out of range: ${value}`);
  }
  const out = new Uint8Array(4);
  new DataView(out.buffer).setInt32(0, value, true);
  return out;
}

/** Little-endian u64. */
export function u64le(value: bigint): Uint8Array {
  if (typeof value !== 'bigint' || value < 0n || value > 0xffff_ffff_ffff_ffffn) {
    throw new RangeError(`u64 out of range: ${String(value)}`);
  }
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, value, true);
  return out;
}

/** Bitcoin CompactSize (varint) encoding. */
export function compactSize(value: number): Uint8Array {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new RangeError(`CompactSize out of range: ${value}`);
  }
  if (value < 0xfd) return Uint8Array.of(value);
  if (value <= 0xffff) return Uint8Array.of(0xfd, value & 0xff, value >> 8);
  if (value <= 0xffff_ffff) return concatBytes(Uint8Array.of(0xfe), u32le(value));
  return concatBytes(Uint8Array.of(0xff), u64le(BigInt(value)));
}

/** Byte string with a CompactSize length prefix. */
export function varBytes(bytes: Uint8Array): Uint8Array {
  return concatBytes(compactSize(bytes.length), bytes);
}

/** Sequential reader over a byte string; reading past the end always throws. */
export class ByteReader {
  private offset = 0;

  constructor(
    private readonly bytes: Uint8Array,
    private readonly what = 'data',
  ) {}

  /** Number of bytes left. */
  get remaining(): number {
    return this.bytes.length - this.offset;
  }

  /** Current read position. */
  get position(): number {
    return this.offset;
  }

  /** Read `length` bytes (returns an independent `Uint8Array` copy, also when the input is a Node.js `Buffer`). */
  read(length: number): Uint8Array {
    if (!Number.isSafeInteger(length) || length < 0 || length > this.remaining) {
      throw new Error(`${this.what} truncated: ${length} bytes needed, ${this.remaining} left`);
    }
    const out = new Uint8Array(length);
    out.set(this.bytes.subarray(this.offset, this.offset + length));
    this.offset += length;
    return out;
  }

  /** Read one byte. */
  u8(): number {
    return this.read(1)[0]!;
  }

  /** Read a little-endian u32. */
  u32(): number {
    const b = this.read(4);
    return new DataView(b.buffer).getUint32(0, true);
  }

  /** Read a little-endian i32. */
  i32(): number {
    const b = this.read(4);
    return new DataView(b.buffer).getInt32(0, true);
  }

  /** Read a little-endian u64. */
  u64(): bigint {
    const b = this.read(8);
    return new DataView(b.buffer).getBigUint64(0, true);
  }

  /** Read a CompactSize; the shortest encoding is required (like Bitcoin Core's `ReadCompactSize`). */
  compactSize(): number {
    const first = this.u8();
    if (first < 0xfd) return first;
    if (first === 0xfd) {
      const b = this.read(2);
      const value = b[0]! | (b[1]! << 8);
      if (value < 0xfd) throw new Error(`CompactSize in ${this.what} is not the shortest encoding`);
      return value;
    }
    if (first === 0xfe) {
      const value = this.u32();
      if (value <= 0xffff) throw new Error(`CompactSize in ${this.what} is not the shortest encoding`);
      return value;
    }
    const value = this.u64();
    if (value <= 0xffff_ffffn) throw new Error(`CompactSize in ${this.what} is not the shortest encoding`);
    if (value > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error(`CompactSize in ${this.what} is too large`);
    return Number(value);
  }

  /** Read a byte string with a CompactSize length prefix. */
  varBytes(): Uint8Array {
    return this.read(this.compactSize());
  }

  /** Require that the whole input has been consumed. */
  finish(): void {
    if (this.remaining !== 0) {
      throw new Error(`${this.remaining} extra bytes after ${this.what}`);
    }
  }
}
