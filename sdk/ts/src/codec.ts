/**
 * Borsh codec and JSON representation of the consensus types (SPEC §4.1, §6.1, §7.1; D-001 to D-007).
 *
 * The type definitions correspond field by field to `crates/ksana-primitives/src/types.rs`: the field order and the
 * variant order are the encoding format. External input is always checked against the length limit before decoding and
 * must be fully consumed (`decodeVmOp` uses `MAX_OP_BYTES`, `decodePayload` uses `MAX_BLOCK_DATA_BYTES`, D-005). The
 * header sequence in a payload uses the compact encoding (`compact.ts`, D-075, D-079), and so do the bodies and entries
 * (`compactEntries.ts`, D-080: a VmOp is written without its chain ID); carried bodies are checked for canonicity when
 * decoding (`checkBodies`, D-077).
 */

import { sha256 } from '@noble/hashes/sha2.js';

import { ByteReader, concatBytes } from './bytes.js';
import { encodeHeaders, readHeaders } from './compact.js';
import { EntryDecoder, EntryEncoder } from './compactEntries.js';
import { MAX_CALL_FUNDS, type Params } from './params.js';
import {
  CodecError,
  type Json,
  type TypeDef,
  decodeWith,
  encodeWith,
  enumType,
  fixedBytes,
  option,
  str,
  struct,
  structVariant,
  tupleVariant,
  u128,
  u32,
  u64,
  u8,
  unitVariant,
  vec,
  vecU8,
} from './schema.js';
import type {
  Action,
  Block,
  BlockHeader,
  Body,
  BtcAnchor,
  Entry,
  EntryData,
  FeeMode,
  Funds,
  OutPoint,
  Payload,
  SettlementTxs,
  SigScheme,
  UnsignedVmOp,
  VmOp,
} from './types.js';

const bytes32 = fixedBytes(32);

const OutPointDef = struct<OutPoint>([
  ['txid', bytes32],
  ['vout', u32],
]);

const FeeModeDef = enumType<FeeMode>([
  structVariant('Token', [['amount', u128]]),
  unitVariant('BtcNative'),
]);

const FundsDef = struct<Funds>([
  ['asset', bytes32],
  ['amount', u128],
]);

const ActionDef = enumType<Action>([
  structVariant('Deploy', [
    ['code', vecU8],
    ['init_args', vecU8],
    ['funds', vec(FundsDef)],
  ]),
  structVariant('Call', [
    ['contract', bytes32],
    ['method', str],
    ['args', vecU8],
    ['funds', vec(FundsDef)],
  ]),
  structVariant('AssetTransfer', [
    ['asset', bytes32],
    ['to', bytes32],
    ['amount', u128],
  ]),
]);

const SigSchemeDef = enumType<SigScheme>([
  tupleVariant('Bip340', 'bytes', fixedBytes(64)),
  tupleVariant('Bip322Simple', 'bytes', vecU8),
]);

const unsignedVmOpFields = [
  ['version', u8],
  ['chain_id', u32],
  ['account', bytes32],
  ['nonce', u64],
  ['fee', FeeModeDef],
  ['max_gas', u64],
  ['action', ActionDef],
] as const;

/** The part of a VmOp other than `sig` (`sig` is the last field: full encoding = this part ‖ `borsh(sig)`). */
const UnsignedVmOpDef = struct<UnsignedVmOp>(unsignedVmOpFields);

const VmOpDef = struct<VmOp>([...unsignedVmOpFields, ['sig', SigSchemeDef]]);

const BtcAnchorDef = struct<BtcAnchor>([
  ['height', u32],
  ['hash', bytes32],
]);

const BlockHeaderDef = struct<BlockHeader>([
  ['version', u32],
  ['height', u64],
  ['parents', vec(bytes32)],
  ['btc_anchor', BtcAnchorDef],
  ['entries_root', bytes32],
  ['state_root', bytes32],
  ['producer', bytes32],
  ['timestamp', u64],
  ['bits', u32],
  ['nonce', u64],
]);

const EntryDef = enumType<Entry>([
  structVariant('Forced', [
    ['btc_txid', bytes32],
    ['op', VmOpDef],
  ]),
  structVariant('Sponsored', [['op', VmOpDef]]),
  structVariant('Unlock', [
    ['height', u64],
    ['index', u32],
  ]),
]);

const EntryDataDef = enumType<EntryData>([
  structVariant('Forced', [['btc_txid', bytes32]]),
  structVariant('Sponsored', [['op', VmOpDef]]),
  structVariant('Unlock', [
    ['height', u64],
    ['index', u32],
  ]),
]);

const SettlementTxsDef = struct<SettlementTxs>([
  ['commit', vecU8],
  ['reveal', vecU8],
]);

const BlockDef = struct<Block>([
  ['header', BlockHeaderDef],
  ['entries', vec(EntryDef)],
  ['settlement', option(SettlementTxsDef)],
]);

const BodyDef = struct<Body>([
  ['header', u32],
  ['entries', vec(EntryDataDef)],
]);

/** Value validation and JSON representation of a payload: in JSON the header sequence is a list of full block headers. */
const PayloadFieldsDef = enumType<Payload>([
  structVariant('Settlement', [
    ['headers', vec(BlockHeaderDef)],
    ['entries', vec(EntryDataDef)],
    ['bodies', vec(BodyDef)],
  ]),
  structVariant('NativeOp', [['op', VmOpDef]]),
  structVariant('Headers', [
    ['headers', vec(BlockHeaderDef)],
    ['bodies', vec(BodyDef)],
    ['payee', bytes32],
  ]),
]);

/**
 * Canonicity of `bodies` (D-077, same as Rust's `check_bodies`, with the same error messages): indices strictly increasing
 * and pointing at existing headers (a settlement payload cannot point at the last header: this block's entries are in
 * `entries`); a body has at least one entry other than a forced inclusion (a block with only forced-inclusion entries
 * needs no body).
 */
export function checkBodies(headers: readonly BlockHeader[], bodies: readonly Body[], settlement: boolean): void {
  let previous: number | undefined;
  for (const body of bodies) {
    if (previous !== undefined && body.header <= previous) {
      throw new CodecError('Borsh decode failed: body indices must be strictly increasing');
    }
    previous = body.header;
    if (settlement && body.header + 1 === headers.length) {
      throw new CodecError("Borsh decode failed: a body in a settlement payload cannot point at this block's own header");
    }
    if (body.header >= headers.length) {
      throw new CodecError('Borsh decode failed: body index exceeds the number of headers');
    }
    if (body.entries.every((e) => e.kind === 'Forced')) {
      throw new CodecError('Borsh decode failed: a body must contain entries other than forced inclusions');
    }
  }
}

/** Block hash `sha256d(borsh(header))` (`blockHash` in `hash.ts`; that module depends on this one, so it is computed here again to avoid a circular dependency). */
function headerHash(header: BlockHeader): Uint8Array {
  return sha256(sha256(encodeWith(BlockHeaderDef, header)));
}

/**
 * Decode a payload (the variant index and what follows); the chain ID of VmOps is `chainId`, and a header sequence holds
 * at most `maxHeaders` headers (`MAX_SETTLEMENT_HEADERS`, §7.1). Truncation, trailing bytes and other errors are all
 * converted to `CodecError`.
 */
function decodePayloadBytes(bytes: Uint8Array, chainId: number, maxHeaders: number): Payload {
  const tag = bytes[0];
  const body = bytes.subarray(1);
  try {
    const reader = new ByteReader(body, 'payload');
    let payload: Payload;
    switch (tag) {
      case 0: {
        const headers = readHeaders(reader, headerHash, maxHeaders);
        const decoder = new EntryDecoder(chainId);
        const bodies = decoder.bodies(reader);
        const entries = decoder.entries(reader);
        checkBodies(headers, bodies, true);
        payload = { kind: 'Settlement', headers, entries, bodies };
        break;
      }
      case 1:
        return { kind: 'NativeOp', op: decodeWith(VmOpDef, body, Number.MAX_SAFE_INTEGER) };
      case 2: {
        const headers = readHeaders(reader, headerHash, maxHeaders);
        const bodies = new EntryDecoder(chainId).bodies(reader);
        checkBodies(headers, bodies, false);
        // payee account (D-095): the 32 bytes after the bodies
        const payee = reader.read(32);
        payload = { kind: 'Headers', headers, bodies, payee };
        break;
      }
      default:
        throw new CodecError(tag === undefined ? 'Borsh decode failed: empty input' : `Borsh decode failed: unknown payload type ${tag}`);
    }
    if (reader.remaining !== 0) {
      throw new CodecError(`Borsh decode failed: ${reader.remaining} trailing bytes after the payload`);
    }
    return payload;
  } catch (e) {
    if (e instanceof CodecError) throw e;
    throw new CodecError(`Borsh decode failed: ${e instanceof Error ? e.message : String(e)}`, { cause: e });
  }
}

/**
 * Payload (SPEC §7.1, D-075, D-077, D-079, D-080, D-095): the variant index (`u8`) followed by the fields; in `Settlement`
 * and `Headers` the header sequence is compressed per `compact.ts` and the bodies and entries per `compactEntries.ts`
 * (`Settlement` writes the bodies first, then this block's entries, sharing one key dictionary; a VmOp is written without
 * its chain ID and is encoded / decoded with `chainId`); `Headers` ends with the 32-byte payee account `payee` (D-095);
 * `NativeOp` is plain Borsh. A payload cannot be nested inside another type.
 */
function payloadDef(params: Params): TypeDef<Payload> {
  const chainId = params.chain_id;
  return {
    ...PayloadFieldsDef,
    codec: {
      encode(value, path) {
        PayloadFieldsDef.toBorsh(value, path); // validate field by field
        switch (value.kind) {
          case 'Settlement': {
            const encoder = new EntryEncoder(chainId);
            return concatBytes(
              Uint8Array.of(0),
              encodeHeaders(value.headers, headerHash),
              encoder.encodeBodies(value.bodies),
              encoder.encodeEntries(value.entries),
            );
          }
          case 'NativeOp':
            return concatBytes(Uint8Array.of(1), encodeWith(VmOpDef, value.op));
          case 'Headers':
            return concatBytes(
              Uint8Array.of(2),
              encodeHeaders(value.headers, headerHash),
              new EntryEncoder(chainId).encodeBodies(value.bodies),
              value.payee,
            );
        }
      },
      decode: (bytes) => decodePayloadBytes(bytes, chainId, params.max_settlement_headers),
    },
  };
}

/**
 * The payload in the generic functions: only its JSON representation (`toJson` / `fromJson`). Its encoding depends on the
 * network (the VmOps in `Settlement` and `Headers` are written without their chain ID, D-080), so it has no Borsh codec here:
 * `encodePayload` / `decodePayload` take the network's parameters (A2-11, D-106).
 */
const PayloadDef: TypeDef<Payload> = {
  ...PayloadFieldsDef,
  codec: {
    encode() {
      throw new TypeError("a payload's encoding depends on the network: use encodePayload(payload, params)");
    },
    decode() {
      throw new TypeError("a payload's encoding depends on the network: use decodePayload(bytes, params)");
    },
  },
};

/** Map from type name (the same as `type` in `tests/vectors/codec.json`) to TS type. */
export interface TypeMap {
  OutPoint: OutPoint;
  FeeMode: FeeMode;
  Funds: Funds;
  Action: Action;
  SigScheme: SigScheme;
  VmOp: VmOp;
  BtcAnchor: BtcAnchor;
  BlockHeader: BlockHeader;
  Entry: Entry;
  EntryData: EntryData;
  Block: Block;
  Payload: Payload;
}

/** Name of a consensus type. */
export type TypeName = keyof TypeMap;

/** Name of a consensus type whose Borsh encoding is the same on every network: all but the payload (`encodePayload`). */
export type BorshTypeName = Exclude<TypeName, 'Payload'>;

const DEFS: { readonly [K in TypeName]: TypeDef<TypeMap[K]> } = {
  OutPoint: OutPointDef,
  FeeMode: FeeModeDef,
  Funds: FundsDef,
  Action: ActionDef,
  SigScheme: SigSchemeDef,
  VmOp: VmOpDef,
  BtcAnchor: BtcAnchorDef,
  BlockHeader: BlockHeaderDef,
  Entry: EntryDef,
  EntryData: EntryDataDef,
  Block: BlockDef,
  Payload: PayloadDef,
};

/** All consensus type names. */
export const TYPE_NAMES: readonly TypeName[] = Object.freeze(Object.keys(DEFS) as TypeName[]);

function defOf<K extends TypeName>(type: K): TypeDef<TypeMap[K]> {
  const def = DEFS[type] as TypeDef<TypeMap[K]> | undefined;
  if (def === undefined) throw new TypeError(`unknown type ${String(type)}`);
  return def;
}

/** Borsh encoding (a payload is encoded with `encodePayload`). */
export function encode<K extends BorshTypeName>(type: K, value: TypeMap[K]): Uint8Array {
  return encodeWith(defOf(type), value);
}

/**
 * Borsh decoding: first checks that the length does not exceed `maxLength`; the input must be fully consumed.
 * For a VmOp or payload from external input use `decodeVmOp` / `decodePayload` (they carry the protocol limits; a payload has
 * no other decoding).
 */
export function decode<K extends BorshTypeName>(type: K, bytes: Uint8Array, maxLength: number): TypeMap[K] {
  return decodeWith(defOf(type), bytes, maxLength);
}

/** The JSON representation of D-007. */
export function toJson<K extends TypeName>(type: K, value: TypeMap[K]): Json {
  return defOf(type).toJson(value, '$');
}

/** Build from the D-007 JSON representation (rejects unknown fields, missing fields and non-canonical decimal notation). */
export function fromJson<K extends TypeName>(type: K, json: unknown): TypeMap[K] {
  return defOf(type).fromJson(json, '$');
}

function compareBytes(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    const d = a[i]! - b[i]!;
    if (d !== 0) return d;
  }
  return a.length - b.length;
}

/**
 * Rules for attached funds (D-073, same as Rust's `check_funds`): at most `MAX_CALL_FUNDS` items, each amount greater
 * than 0, asset IDs strictly ascending (no duplicates). Throws `RangeError` when violated. The codec itself does not check
 * these rules: an op that violates them fails at execution.
 */
export function checkFunds(funds: readonly Funds[]): void {
  if (funds.length > MAX_CALL_FUNDS) throw new RangeError(`attached funds exceed ${MAX_CALL_FUNDS} items`);
  funds.forEach((f, i) => {
    if (f.amount <= 0n) throw new RangeError('attached fund amounts must be greater than 0');
    const prev = funds[i - 1];
    if (prev !== undefined && compareBytes(prev.asset, f.asset) >= 0) {
      throw new RangeError('attached fund asset IDs must be strictly ascending (no duplicates)');
    }
  });
}

/** A copy sorted by asset ID in ascending order (duplicate assets stay duplicated, for `checkFunds` to reject). */
export function sortFunds(funds: readonly Funds[]): Funds[] {
  return [...funds].sort((a, b) => compareBytes(a.asset, b.asset));
}

/** Full Borsh encoding of a VmOp (including `sig`). */
export function encodeVmOp(op: VmOp): Uint8Array {
  return encodeWith(VmOpDef, op);
}

/** Content of the signing message: the Borsh encoding of a VmOp without the `sig` field (SPEC §4.2). */
export function encodeVmOpUnsigned(op: UnsignedVmOp): Uint8Array {
  return encodeWith(UnsignedVmOpDef, op);
}

/** Decode a single VmOp: the encoded length must not exceed `MAX_OP_BYTES` of the network's `params` (SPEC §4.4). */
export function decodeVmOp(bytes: Uint8Array, params: Params): VmOp {
  return decodeWith(VmOpDef, bytes, params.max_op_bytes);
}

/**
 * Encode a payload (headers and entries compressed, D-075, D-079, D-080) for the network of `params`. The VmOps in
 * `Settlement` and `Headers` are written without their chain ID: it must be `params.chain_id`, otherwise `CodecError` is
 * thrown.
 */
export function encodePayload(payload: Payload, params: Params): Uint8Array {
  return encodeWith(payloadDef(params), payload);
}

/**
 * Decode the payload in a BTC transaction of the network of `params`: the length must not exceed `MAX_BLOCK_DATA_BYTES`
 * (D-005), a header sequence holds at most `MAX_SETTLEMENT_HEADERS` headers (§7.1) and the input must be fully consumed; the
 * VmOps in `Settlement` and `Headers` get the chain ID `params.chain_id` (they are written without it, so decoding with
 * another network's parameters would silently give them the wrong one).
 */
export function decodePayload(bytes: Uint8Array, params: Params): Payload {
  return decodeWith(payloadDef(params), bytes, params.max_block_data_bytes);
}

/** Borsh encoded length of a block header without parents (the genesis block): 4+8+4+(4+32)+32+32+32+8+4+8 = 168 bytes; plus 32 bytes per parent (D-063). */
export const BLOCK_HEADER_BASE_LEN = 168;

/** Borsh encoded length of a block header. */
export function blockHeaderEncodedLen(header: BlockHeader): number {
  return BLOCK_HEADER_BASE_LEN + 32 * header.parents.length;
}

/** Borsh encoding of a block header (`168 + 32 × number of parents` bytes). */
export function encodeBlockHeader(header: BlockHeader): Uint8Array {
  return encodeWith(BlockHeaderDef, header);
}

/** Borsh encoding of a block entry (a leaf of `entries_root`). */
export function encodeEntry(entry: Entry): Uint8Array {
  return encodeWith(EntryDef, entry);
}

/** JSON representation of a VmOp (D-007). */
export function vmOpToJson(op: VmOp): Json {
  return VmOpDef.toJson(op, '$');
}

/** Build a VmOp from its JSON representation (D-007). */
export function vmOpFromJson(json: unknown): VmOp {
  return VmOpDef.fromJson(json, '$');
}
