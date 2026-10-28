/**
 * Compact encoding of the entries in a payload (D-080, revising SPEC §7.1 and D-003), byte-for-byte identical to
 * `crates/ksana-primitives/src/compact_entries.rs`: the bodies and this block's entries in `Settlement` and `Headers`
 * payloads are encoded in the format below, and decoding restores the full entries: the `EntryData`, the VmOps in them
 * and their Borsh encoding, signatures, `op_id` and `entries_root` are all unchanged.
 *
 * | Element | Encoding |
 * | --- | --- |
 * | entry | `u8` variant (0 forced inclusion, 1 path B, 2 unlock), followed by that variant's fields |
 * | forced inclusion | the 32-byte BTC transaction ID |
 * | unlock | varint height, varint acceptance index (D-079) |
 * | path B | the VmOp: `version: u8`, key `account`, varint `nonce`, fee, varint `max_gas`, action, signature; `chain_id` is not written and is taken from the network parameters when decoding |
 * | fee | `u8` variant: 0 `Token` followed by the varint amount; 1 `BtcNative` |
 * | action | `u8` variant: 0 `Deploy` (byte string `code`, byte string `init_args`, asset list); 1 `Call` (key `contract`, string `method`, byte string `args`, asset list); 2 `AssetTransfer` (key `asset`, key `to`, varint `amount`) |
 * | asset list | varint count, each item a key `asset` and a varint `amount` |
 * | signature | `u8` variant: 0 `Bip340` followed by 64 bytes; 1 `Bip322Simple` followed by a byte string |
 * | key | varint `r`: `r ≥ 1` is the `r`-th key in the dictionary; `r = 0` is followed by the 32 bytes, which are appended to the end of the dictionary |
 * | byte string, string | varint length and the content; a string must be valid UTF-8 |
 * | entry list | varint count and the entries |
 * | body list | varint count, each body a varint header index and an entry list (D-077) |
 *
 * All integers are LEB128 varints (unsigned, shortest encoding, within the field's type). Keys are the 32-byte accounts,
 * contract IDs, asset IDs and recipients: the bodies and entries of one payload share one dictionary, ordered by first
 * occurrence and preset with the KSA asset ID (the 1st key). The encoding is canonical: a key already in the dictionary
 * must be referenced, a reference cannot be out of range, and a varint must be the shortest encoding. The error messages
 * are the same as on the Rust side (the `error` of the shared vectors `tests/vectors/codec.json`).
 */

import { ByteReader, concatBytes, toHex, utf8 } from './bytes.js';
import { invalid, readVarint, writeVarint } from './compact.js';
import { CodecError } from './schema.js';
import type { Action, Body, EntryData, FeeMode, Funds, SigScheme, VmOp } from './types.js';

const FORCED = 0;
const SPONSORED = 1;
const UNLOCK = 2;
const FEE_TOKEN = 0;
const FEE_NATIVE = 1;
const ACTION_DEPLOY = 0;
const ACTION_CALL = 1;
const ACTION_TRANSFER = 2;
const SIG_BIP340 = 0;
const SIG_BIP322 = 1;
const U32_MAX = 0xffff_ffffn;
const U128_MAX = (1n << 128n) - 1n;
/** The KSA asset ID: 32 zero bytes, the 1st key preset in the dictionary. */
const KSA_KEY = new Uint8Array(32);

// `ignoreBOM`: a leading U+FEFF is part of the method name, as in Rust's `String::from_utf8`; the default would drop it
// and then reject a payload that Rust accepts (the re-encoding comparison fails)
const UTF8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** LEB128 for `u128`; the shortest encoding is required and the value must fit in `u128`. */
function readVarintU128(reader: ByteReader): bigint {
  let value = 0n;
  let shift = 0n;
  for (;;) {
    const byte = reader.u8();
    const low = BigInt(byte & 0x7f);
    // the 19th byte (shift 126) only has room for 2 bits
    if (shift === 126n && low > 3n) invalid('varint exceeds u128');
    value |= low << shift;
    if ((byte & 0x80) === 0) {
      if (byte === 0 && shift > 0n) invalid('varint is not minimally encoded');
      return value;
    }
    shift += 7n;
    if (shift > 126n) invalid('varint too long');
  }
}

function readVarintU32(reader: ByteReader): number {
  const value = readVarint(reader);
  if (value > U32_MAX) invalid('varint exceeds u32');
  return Number(value);
}

/** Counts and lengths: at most the number of remaining bytes (each element takes at least 1 byte), otherwise the input is necessarily truncated. */
function readCount(reader: ByteReader): number {
  const value = readVarint(reader);
  if (value > BigInt(reader.remaining)) {
    throw new Error(`payload truncated: count ${value} exceeds the ${reader.remaining} remaining bytes`);
  }
  return Number(value);
}

function readBytes(reader: ByteReader): Uint8Array {
  return reader.read(readCount(reader));
}

function checkUnsigned(value: bigint, max: bigint, what: string): void {
  if (typeof value !== 'bigint' || value < 0n || value > max) {
    throw new CodecError(`Borsh encode failed: ${what} out of range`);
  }
}

/**
 * Entry encoder (D-080): holds the key dictionary of a payload and encodes the bodies and entries in payload order.
 * `encodedLength` only computes the length and does not change the dictionary.
 */
export class EntryEncoder {
  /** Key (hex) → reference value `r` (its index in the dictionary plus 1). */
  private readonly keys = new Map<string, bigint>([[toHex(KSA_KEY), 1n]]);

  /** `chainId` is the network's chain ID: VmOps in a payload do not write it, and a VmOp with a different chain ID cannot be encoded. */
  constructor(private readonly chainId: number) {}

  private nextRef(): bigint {
    return BigInt(this.keys.size) + 1n;
  }

  private write(entry: EntryData): { bytes: Uint8Array; newKeys: string[] } {
    const out: number[] = [];
    const newKeys: string[] = [];
    const push = (bytes: Uint8Array): void => {
      for (const b of bytes) out.push(b);
    };
    const varint = (value: bigint): void => writeVarint(out, value);
    const bytes = (value: Uint8Array): void => {
      varint(BigInt(value.length));
      push(value);
    };
    const key = (value: Uint8Array): void => {
      if (value.length !== 32) throw new CodecError('Borsh encode failed: a key must be 32 bytes');
      const hex = toHex(value);
      const known = this.keys.get(hex);
      if (known !== undefined) return varint(known);
      const position = newKeys.indexOf(hex);
      if (position >= 0) return varint(this.nextRef() + BigInt(position));
      varint(0n);
      push(value);
      newKeys.push(hex);
    };
    const funds = (list: readonly Funds[]): void => {
      varint(BigInt(list.length));
      for (const f of list) {
        key(f.asset);
        checkUnsigned(f.amount, U128_MAX, 'attached fund amount');
        varint(f.amount);
      }
    };
    const fee = (value: FeeMode): void => {
      if (value.kind === 'Token') {
        out.push(FEE_TOKEN);
        checkUnsigned(value.amount, U128_MAX, 'token fee');
        varint(value.amount);
      } else {
        out.push(FEE_NATIVE);
      }
    };
    const action = (value: Action): void => {
      switch (value.kind) {
        case 'Deploy':
          out.push(ACTION_DEPLOY);
          bytes(value.code);
          bytes(value.init_args);
          funds(value.funds);
          return;
        case 'Call':
          out.push(ACTION_CALL);
          key(value.contract);
          bytes(utf8(value.method));
          bytes(value.args);
          funds(value.funds);
          return;
        case 'AssetTransfer':
          out.push(ACTION_TRANSFER);
          key(value.asset);
          key(value.to);
          checkUnsigned(value.amount, U128_MAX, 'transfer amount');
          varint(value.amount);
          return;
      }
    };
    const sig = (value: SigScheme): void => {
      if (value.kind === 'Bip340') {
        if (value.bytes.length !== 64) throw new CodecError('Borsh encode failed: a BIP340 signature must be 64 bytes');
        out.push(SIG_BIP340);
        push(value.bytes);
      } else {
        out.push(SIG_BIP322);
        bytes(value.bytes);
      }
    };
    const op = (value: VmOp): void => {
      if (value.chain_id !== this.chainId) {
        throw new CodecError('Borsh encode failed: VmOp chain_id does not match the network and cannot be written into a payload');
      }
      out.push(value.version);
      key(value.account);
      varint(value.nonce);
      fee(value.fee);
      varint(value.max_gas);
      action(value.action);
      sig(value.sig);
    };
    switch (entry.kind) {
      case 'Forced':
        out.push(FORCED);
        push(entry.btc_txid);
        break;
      case 'Sponsored':
        out.push(SPONSORED);
        op(entry.op);
        break;
      case 'Unlock':
        out.push(UNLOCK);
        varint(entry.height);
        checkUnsigned(BigInt(entry.index), U32_MAX, 'acceptance index');
        varint(BigInt(entry.index));
        break;
    }
    return { bytes: Uint8Array.from(out), newKeys };
  }

  /** Encoding of one entry; keys first seen in it are appended to the dictionary. */
  encode(entry: EntryData): Uint8Array {
    const { bytes, newKeys } = this.write(entry);
    for (const hex of newKeys) this.keys.set(hex, this.nextRef());
    return bytes;
  }

  /** Encoded length of one entry under the current dictionary (does not change the dictionary). */
  encodedLength(entry: EntryData): number {
    return this.write(entry).bytes.length;
  }

  /** Entry list (varint count and the entries). */
  encodeEntries(entries: readonly EntryData[]): Uint8Array {
    const count: number[] = [];
    writeVarint(count, BigInt(entries.length));
    return concatBytes(Uint8Array.from(count), ...entries.map((e) => this.encode(e)));
  }

  /** Body list (varint count, each body a varint header index and an entry list). */
  encodeBodies(bodies: readonly Body[]): Uint8Array {
    const parts: Uint8Array[] = [];
    const count: number[] = [];
    writeVarint(count, BigInt(bodies.length));
    parts.push(Uint8Array.from(count));
    for (const body of bodies) {
      checkUnsigned(BigInt(body.header), U32_MAX, 'body header index');
      const header: number[] = [];
      writeVarint(header, BigInt(body.header));
      parts.push(Uint8Array.from(header), this.encodeEntries(body.entries));
    }
    return concatBytes(...parts);
  }
}

/** Entry decoder (D-080): the counterpart of `EntryEncoder`; reads the bodies and entries in the same order and checks that the encoding is canonical. */
export class EntryDecoder {
  private readonly keys: Uint8Array[] = [KSA_KEY];
  private readonly known = new Set<string>([toHex(KSA_KEY)]);

  /** The chain ID of the restored VmOps is `chainId` (a network parameter). */
  constructor(private readonly chainId: number) {}

  private key(reader: ByteReader): Uint8Array {
    const r = readVarint(reader);
    if (r === 0n) {
      const value = reader.read(32);
      const hex = toHex(value);
      if (this.known.has(hex)) invalid('a key already in the dictionary must be referenced (non-canonical encoding)');
      this.known.add(hex);
      this.keys.push(value);
      return value;
    }
    if (r > BigInt(this.keys.length)) invalid('key reference out of range');
    return this.keys[Number(r) - 1]!.slice();
  }

  private funds(reader: ByteReader): Funds[] {
    const count = readCount(reader);
    const out: Funds[] = [];
    for (let i = 0; i < count; i++) out.push({ asset: this.key(reader), amount: readVarintU128(reader) });
    return out;
  }

  private action(reader: ByteReader): Action {
    switch (reader.u8()) {
      case ACTION_DEPLOY:
        return { kind: 'Deploy', code: readBytes(reader), init_args: readBytes(reader), funds: this.funds(reader) };
      case ACTION_CALL: {
        const contract = this.key(reader);
        let method: string;
        try {
          method = UTF8.decode(readBytes(reader));
        } catch (e) {
          if (e instanceof CodecError) throw e;
          if (e instanceof TypeError) invalid('method name is not valid UTF-8');
          throw e;
        }
        return { kind: 'Call', contract, method, args: readBytes(reader), funds: this.funds(reader) };
      }
      case ACTION_TRANSFER:
        return { kind: 'AssetTransfer', asset: this.key(reader), to: this.key(reader), amount: readVarintU128(reader) };
      default:
        return invalid('unknown action');
    }
  }

  private op(reader: ByteReader): VmOp {
    const version = reader.u8();
    const account = this.key(reader);
    const nonce = readVarint(reader);
    let fee: FeeMode;
    switch (reader.u8()) {
      case FEE_TOKEN:
        fee = { kind: 'Token', amount: readVarintU128(reader) };
        break;
      case FEE_NATIVE:
        fee = { kind: 'BtcNative' };
        break;
      default:
        return invalid('unknown fee mode');
    }
    const max_gas = readVarint(reader);
    const action = this.action(reader);
    let sig: SigScheme;
    switch (reader.u8()) {
      case SIG_BIP340:
        sig = { kind: 'Bip340', bytes: reader.read(64) };
        break;
      case SIG_BIP322:
        sig = { kind: 'Bip322Simple', bytes: readBytes(reader) };
        break;
      default:
        return invalid('unknown signature scheme');
    }
    return { version, chain_id: this.chainId, account, nonce, fee, max_gas, action, sig };
  }

  /** Read one entry. */
  entry(reader: ByteReader): EntryData {
    switch (reader.u8()) {
      case FORCED:
        return { kind: 'Forced', btc_txid: reader.read(32) };
      case SPONSORED:
        return { kind: 'Sponsored', op: this.op(reader) };
      case UNLOCK:
        return { kind: 'Unlock', height: readVarint(reader), index: readVarintU32(reader) };
      default:
        return invalid('unknown entry variant');
    }
  }

  /** Read an entry list. */
  entries(reader: ByteReader): EntryData[] {
    const count = readCount(reader);
    const out: EntryData[] = [];
    for (let i = 0; i < count; i++) out.push(this.entry(reader));
    return out;
  }

  /** Read a body list (the caller checks canonicity separately with `checkBodies`). */
  bodies(reader: ByteReader): Body[] {
    const count = readCount(reader);
    const out: Body[] = [];
    for (let i = 0; i < count; i++) {
      const header = readVarintU32(reader);
      out.push({ header, entries: this.entries(reader) });
    }
    return out;
  }
}
