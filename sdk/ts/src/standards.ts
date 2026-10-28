/**
 * The token standards KS-20 (fungible tokens) and KS-721 (non-fungible tokens) of `docs/STANDARDS.md` (D-120): conventions
 * between contracts, wallets and indexers, not consensus rules.
 *
 * - Discovery: `readStandards` asks a contract for its declaration (`standards()`), `standardOf` names the standard a
 *   declaration is of.
 * - Operations: `ks20Transfer`, `ks20Approve`, `ks20TransferAndCall`, ... and `ks721Transfer`, `ks721SetOperator`, ... build
 *   the `Action` of a VmOp, signed and submitted like any other; `assetTransfer` moves an `AssetRef` of either kind. They refuse
 *   what every token refuses (a zero amount, the zero account or the contract itself as the recipient), which would only cost a
 *   fee on chain.
 * - Views, through the node's read-only call (`ksana_call`): `readKs20` / `readKs721` (the declaration, the metadata, the
 *   capabilities), and one function per view (`ks20BalanceOf`, `ks721OwnerOf`, ...). A contract that refuses throws
 *   `StandardCallError` with its reason; an answer that does not decode throws `CodecError`. A node runs at most 4 calls at a
 *   time, so the functions that read several views read them one after another.
 * - Events: `decodeKs20Event` / `decodeKs721Event` (`null` for bytes that are no event of the standard: another name, other
 *   fields, bytes left over), `decodeStandardEvent` by a contract's declaration, and the encoders.
 * - KS-721 token IDs from serial numbers (`ks721TokenId` / `ks721Serial`), and `AssetRef`, a fungible asset of either kind.
 *
 * Every encoding is the contract SDK's (`crates/ksana-contract-sdk`, feature `standards`), checked against the shared vectors
 * `tests/vectors/standards.json`. Fields are named as the standard names them (`expires_at`, `token_id`), and the kinds of an
 * event as the Rust variants.
 */

import { assertBytes, equalBytes } from './bytes.js';
import type { CallRequest, Receipt } from './rpc.js';
import {
  type TypeDef,
  bool,
  CodecError,
  decodeWith,
  encodeWith,
  fixedBytes,
  option,
  str,
  struct,
  u128,
  u32,
  u64,
  u8,
  vec,
  vecU8,
} from './schema.js';
import type { Action, Funds } from './types.js';

/** KS-20, fungible tokens. */
export const KS20 = 'KS-20';
/** The KS-20 extension `burn` / `burn_from`. */
export const KS20_BURNABLE = 'KS-20-burnable';
/** The KS-20 extension of a minter and a cap. */
export const KS20_MINTABLE = 'KS-20-mintable';
/** KS-721, non-fungible tokens. */
export const KS721 = 'KS-721';
/** The KS-721 extension `burn`. */
export const KS721_BURNABLE = 'KS-721-burnable';
/** The KS-721 extension of the lists of tokens, `total_supply` / `tokens` / `tokens_of`. */
export const KS721_ENUMERABLE = 'KS-721-enumerable';

/** The two standards: the first ID of a declaration. */
export type Standard = typeof KS20 | typeof KS721;

/**
 * The capabilities a contract reports in `capabilities()` while they hold, in the order it reports them (`docs/STANDARDS.md` §5.2).
 * The first four are also the names, in this order, of the issuer capabilities planned for built-in assets; `clawback`, `fee` and
 * `upgrade` only contracts have, and a contract may report names of its own after these.
 */
export const STANDARD_CAPABILITIES = Object.freeze(['mint', 'freeze', 'allowlist', 'pause', 'clawback', 'fee', 'upgrade'] as const);

/** A KS-20 allowance that never goes down when it is spent (`u128::MAX`). */
export const KS20_UNLIMITED = (1n << 128n) - 1n;
/** The most decimals a KS-20 token has. */
export const KS20_MAX_DECIMALS = 18;
/** The most bytes of a token's or a collection's name (UTF-8), at least 1. */
export const MAX_TOKEN_NAME_BYTES = 64;
/** The most bytes of a token's or a collection's symbol (UTF-8), at least 1. */
export const MAX_TOKEN_SYMBOL_BYTES = 16;
/** The most bytes of a token URI the reference implementation stores. */
export const KS721_MAX_URI_BYTES = 2_048;
/** The most token IDs a page of `tokens` / `tokens_of` holds. */
export const KS721_MAX_PAGE = 100;

/** What starts the reason of a refusal by a contract's own rules (`rejected: <reason>`). */
export const REJECTED_PREFIX = 'rejected: ';

/**
 * The revert reasons the standards fix (`docs/STANDARDS.md` §5.1), for wallets to translate; a refusal by a contract's own rules
 * starts with `REJECTED_PREFIX`.
 */
export const STANDARD_REASONS = Object.freeze([
  'unknown method',
  'malformed arguments',
  'unexpected attached funds',
  'zero amount',
  'zero account',
  'recipient is this contract',
  'insufficient balance',
  'insufficient allowance',
  'allowance expired',
  'spender is the caller',
  'unknown token',
  'not the owner',
  'not authorized',
  'wrong owner',
  'already the owner',
  'spender is the owner',
  'operator is the caller',
  'token exists',
  'token was burned',
  'malformed return value',
  'not the minter',
  'minting has ended',
  'cap exceeded',
] as const);

/** The most bytes of an event (the VM's `MAX_EVENT_BYTES`, `docs/ABI.md`). */
const MAX_EVENT_BYTES = 16_384;
/** The most bytes of a call's answer (the VM's `MAX_OUTPUT_BYTES`). */
const MAX_OUTPUT_BYTES = 65_536;
/** The longest name of a standard event. */
const MAX_EVENT_NAME_BYTES = 16;

const ZERO_ACCOUNT = new Uint8Array(32);

const account = fixedBytes(32);
const optAccount = option(account);
const optU64 = option(u64);
const strings = vec(str);

// ---------------------------------------------------------------------------------------------
// Declarations

/** The standard a declaration is of: its first ID, when it is KS-20 or KS-721 and the list does not name the other too. */
export function standardOf(ids: readonly string[]): Standard | null {
  const first = ids[0];
  if (first === KS20) return ids.includes(KS721) ? null : KS20;
  if (first === KS721) return ids.includes(KS20) ? null : KS721;
  return null;
}

/** Whether the time `now` (a block timestamp, in seconds) is past an allowance's or an operator's expiry (`null`: never). */
export function isExpired(expiresAt: bigint | null, now: bigint): boolean {
  return expiresAt !== null && now >= expiresAt;
}

// ---------------------------------------------------------------------------------------------
// KS-721 token IDs and AssetRef

/** The KS-721 token ID of a serial number: the number big-endian in the last 16 of the 32 bytes. */
export function ks721TokenId(serial: bigint): Uint8Array {
  if (typeof serial !== 'bigint' || serial < 0n || serial > KS20_UNLIMITED) {
    throw new RangeError('a serial number is a bigint within u128 range');
  }
  const out = new Uint8Array(32);
  let rest = serial;
  for (let i = 31; i >= 16; i -= 1) {
    out[i] = Number(rest & 0xffn);
    rest >>= 8n;
  }
  return out;
}

/** The serial number a KS-721 token ID holds, or `null` for an ID whose first 16 bytes are not all 0 (a hash, say). */
export function ks721Serial(tokenId: Uint8Array): bigint | null {
  const id = assertBytes(tokenId, 32, 'token ID');
  if (id.subarray(0, 16).some((b) => b !== 0)) return null;
  let serial = 0n;
  for (let i = 16; i < 32; i += 1) serial = (serial << 8n) | BigInt(id[i]!);
  return serial;
}

/**
 * A fungible asset of either kind: a KS-20 token (`Token`, the contract's ID) or a built-in asset (`Asset`, its asset ID; KSA is
 * 32 zero bytes). Its Borsh encoding (`encodeAssetRef`) is the variant's index and the ID, as contracts take it.
 */
export type AssetRef = { kind: 'Token'; id: Uint8Array } | { kind: 'Asset'; id: Uint8Array };

/** The Borsh encoding of an `AssetRef`: 0 for a token, 1 for a built-in asset, then the 32-byte ID. */
export function encodeAssetRef(ref: AssetRef): Uint8Array {
  const tag = ref.kind === 'Token' ? 0 : ref.kind === 'Asset' ? 1 : -1;
  if (tag < 0) throw new TypeError('an AssetRef is a Token or an Asset');
  const out = new Uint8Array(33);
  out[0] = tag;
  out.set(assertBytes(ref.id, 32, 'asset ID'), 1);
  return out;
}

/** Decodes an `AssetRef` (33 bytes, a tag of 0 or 1); throws `CodecError` otherwise. */
export function decodeAssetRef(bytes: Uint8Array): AssetRef {
  const input = assertBytes(bytes, undefined, 'AssetRef');
  if (input.length !== 33) throw new CodecError(`an AssetRef is 33 bytes, got ${input.length}`);
  const id = input.slice(1);
  if (input[0] === 0) return { kind: 'Token', id };
  if (input[0] === 1) return { kind: 'Asset', id };
  throw new CodecError(`unknown AssetRef variant ${input[0]}`);
}

/**
 * Moves `amount` of an asset of either kind from the op's account to `to`: a built-in asset's `AssetTransfer`, or the token's
 * `transfer`. A token that charges a fee gives `to` less than `amount`, and takes exactly `amount` from the sender.
 */
export function assetTransfer(ref: AssetRef, to: Uint8Array, amount: bigint): Action {
  if (ref.kind === 'Token') return ks20Transfer(ref.id, to, amount);
  if (ref.kind !== 'Asset') throw new TypeError('an AssetRef is a Token or an Asset');
  positive(amount);
  return { kind: 'AssetTransfer', asset: assertBytes(ref.id, 32, 'asset ID'), to: assertBytes(to, 32, 'recipient'), amount };
}

// ---------------------------------------------------------------------------------------------
// Operations

function call(contract: Uint8Array, method: string, args: Uint8Array, funds: readonly Funds[] = []): Action {
  return { kind: 'Call', contract: assertBytes(contract, 32, 'contract'), method, args, funds: [...funds] };
}

/** An amount the standards move: above 0. */
function positive(amount: bigint): void {
  if (typeof amount !== 'bigint' || amount <= 0n) throw new RangeError('zero amount: the amount must be a bigint above 0');
}

/** A recipient every token takes: not the zero account, not the token itself. */
function recipient(contract: Uint8Array, to: Uint8Array): Uint8Array {
  const out = assertBytes(to, 32, 'recipient');
  if (equalBytes(out, ZERO_ACCOUNT)) throw new RangeError('zero account: the recipient cannot be the zero account');
  if (equalBytes(out, assertBytes(contract, 32, 'contract'))) throw new RangeError('recipient is this contract');
  return out;
}

const KS20_TRANSFER = struct<{ to: Uint8Array; amount: bigint }>([
  ['to', account],
  ['amount', u128],
]);
const KS20_TRANSFER_FROM = struct<{ from: Uint8Array; to: Uint8Array; amount: bigint }>([
  ['from', account],
  ['to', account],
  ['amount', u128],
]);
const KS20_APPROVE = struct<{ spender: Uint8Array; amount: bigint; expires_at: bigint | null }>([
  ['spender', account],
  ['amount', u128],
  ['expires_at', optU64],
]);
const KS20_TRANSFER_AND_CALL = struct<{ to: Uint8Array; amount: bigint; data: Uint8Array }>([
  ['to', account],
  ['amount', u128],
  ['data', vecU8],
]);
const KS20_BURN_FROM = struct<{ from: Uint8Array; amount: bigint }>([
  ['from', account],
  ['amount', u128],
]);

/** KS-20 `transfer(to, amount)` of `token`. */
export function ks20Transfer(token: Uint8Array, to: Uint8Array, amount: bigint): Action {
  positive(amount);
  return call(token, 'transfer', encodeWith(KS20_TRANSFER, { to: recipient(token, to), amount }));
}

/** KS-20 `transfer_from(from, to, amount)` of `token`: with the allowance `from` gave the op's account, unless it is `from`. */
export function ks20TransferFrom(token: Uint8Array, from: Uint8Array, to: Uint8Array, amount: bigint): Action {
  positive(amount);
  return call(token, 'transfer_from', encodeWith(KS20_TRANSFER_FROM, { from, to: recipient(token, to), amount }));
}

/**
 * KS-20 `approve(spender, amount, expires_at)` of `token`: sets the allowance (it does not add to it); 0 revokes it,
 * `KS20_UNLIMITED` never goes down; `expiresAt` is the block timestamp (seconds) from which it no longer counts, `null` never.
 */
export function ks20Approve(token: Uint8Array, spender: Uint8Array, amount: bigint, expiresAt: bigint | null = null): Action {
  const who = assertBytes(spender, 32, 'spender');
  if (equalBytes(who, ZERO_ACCOUNT)) throw new RangeError('zero account: the spender cannot be the zero account');
  return call(token, 'approve', encodeWith(KS20_APPROVE, { spender: who, amount, expires_at: expiresAt }));
}

/**
 * KS-20 `transfer_and_call(to, amount, data)` of `token`: moves the tokens to the contract `to` and calls its
 * `on_token_received(from, amount, data)`, passing `funds` on to it; the whole op fails when `to` refuses. The receipt's
 * output is `to`'s answer (`decodeTransferAndCallAnswer`).
 */
export function ks20TransferAndCall(
  token: Uint8Array,
  to: Uint8Array,
  amount: bigint,
  data: Uint8Array = new Uint8Array(0),
  funds: readonly Funds[] = [],
): Action {
  positive(amount);
  return call(token, 'transfer_and_call', encodeWith(KS20_TRANSFER_AND_CALL, { to: recipient(token, to), amount, data }), funds);
}

/** `burn(amount)` of a `KS-20-burnable` token: burns the op's account's tokens. */
export function ks20Burn(token: Uint8Array, amount: bigint): Action {
  positive(amount);
  return call(token, 'burn', encodeWith(u128, amount));
}

/** `burn_from(from, amount)` of a `KS-20-burnable` token: with the allowance `from` gave the op's account. */
export function ks20BurnFrom(token: Uint8Array, from: Uint8Array, amount: bigint): Action {
  positive(amount);
  return call(token, 'burn_from', encodeWith(KS20_BURN_FROM, { from, amount }));
}

/** `mint(to, amount)` of a `KS-20-mintable` token: the minter only, up to the cap. */
export function ks20Mint(token: Uint8Array, to: Uint8Array, amount: bigint): Action {
  positive(amount);
  return call(token, 'mint', encodeWith(KS20_TRANSFER, { to: recipient(token, to), amount }));
}

/** `set_minter(minter)` of a `KS-20-mintable` token: the minter only; `null` ends minting for good. */
export function ks20SetMinter(token: Uint8Array, minter: Uint8Array | null): Action {
  if (minter !== null && equalBytes(assertBytes(minter, 32, 'minter'), ZERO_ACCOUNT)) {
    throw new RangeError('zero account: the minter cannot be the zero account (null ends minting)');
  }
  return call(token, 'set_minter', encodeWith(optAccount, minter));
}

const KS721_TRANSFER = struct<{ to: Uint8Array; token_id: Uint8Array }>([
  ['to', account],
  ['token_id', account],
]);
const KS721_TRANSFER_FROM = struct<{ from: Uint8Array; to: Uint8Array; token_id: Uint8Array }>([
  ['from', account],
  ['to', account],
  ['token_id', account],
]);
const KS721_APPROVE = struct<{ spender: Uint8Array | null; token_id: Uint8Array }>([
  ['spender', optAccount],
  ['token_id', account],
]);
const KS721_SET_OPERATOR = struct<{ operator: Uint8Array; approved: boolean; expires_at: bigint | null }>([
  ['operator', account],
  ['approved', bool],
  ['expires_at', optU64],
]);
const KS721_TRANSFER_AND_CALL = struct<{ to: Uint8Array; token_id: Uint8Array; data: Uint8Array }>([
  ['to', account],
  ['token_id', account],
  ['data', vecU8],
]);

/** KS-721 `transfer(to, token_id)` of `collection`: the owner only. */
export function ks721Transfer(collection: Uint8Array, to: Uint8Array, tokenId: Uint8Array): Action {
  return call(collection, 'transfer', encodeWith(KS721_TRANSFER, { to: recipient(collection, to), token_id: tokenId }));
}

/** KS-721 `transfer_from(from, to, token_id)` of `collection`: the owner, the approved account or an operator; `from` must own it. */
export function ks721TransferFrom(collection: Uint8Array, from: Uint8Array, to: Uint8Array, tokenId: Uint8Array): Action {
  const args = encodeWith(KS721_TRANSFER_FROM, { from, to: recipient(collection, to), token_id: tokenId });
  return call(collection, 'transfer_from', args);
}

/** KS-721 `approve(spender, token_id)` of `collection`: the owner or an operator; `null` clears the token's approval. */
export function ks721Approve(collection: Uint8Array, spender: Uint8Array | null, tokenId: Uint8Array): Action {
  if (spender !== null && equalBytes(assertBytes(spender, 32, 'spender'), ZERO_ACCOUNT)) {
    throw new RangeError('zero account: the spender cannot be the zero account (null clears the approval)');
  }
  return call(collection, 'approve', encodeWith(KS721_APPROVE, { spender, token_id: tokenId }));
}

/**
 * KS-721 `set_operator(operator, approved, expires_at)` of `collection`: lets `operator` move all of the op's account's tokens of
 * the collection until `expiresAt` (a block timestamp in seconds, `null` never), or revokes it (`approved` false).
 */
export function ks721SetOperator(
  collection: Uint8Array,
  operator: Uint8Array,
  approved: boolean,
  expiresAt: bigint | null = null,
): Action {
  const who = assertBytes(operator, 32, 'operator');
  if (equalBytes(who, ZERO_ACCOUNT)) throw new RangeError('zero account: the operator cannot be the zero account');
  return call(collection, 'set_operator', encodeWith(KS721_SET_OPERATOR, { operator: who, approved, expires_at: expiresAt }));
}

/**
 * KS-721 `transfer_and_call(to, token_id, data)` of `collection`: the owner only; moves the token to the contract `to` and calls
 * its `on_nft_received(from, token_id, data)`, passing `funds` on to it. The receipt's output is `to`'s answer.
 */
export function ks721TransferAndCall(
  collection: Uint8Array,
  to: Uint8Array,
  tokenId: Uint8Array,
  data: Uint8Array = new Uint8Array(0),
  funds: readonly Funds[] = [],
): Action {
  const args = encodeWith(KS721_TRANSFER_AND_CALL, { to: recipient(collection, to), token_id: tokenId, data });
  return call(collection, 'transfer_and_call', args, funds);
}

/** `burn(token_id)` of a `KS-721-burnable` collection: the owner, the approved account or an operator. */
export function ks721Burn(collection: Uint8Array, tokenId: Uint8Array): Action {
  return call(collection, 'burn', encodeWith(account, tokenId));
}

/** The answer of a `transfer_and_call` (KS-20 or KS-721), the receipt's output: the recipient's answer as it gave it. */
export function decodeTransferAndCallAnswer(output: Uint8Array): Uint8Array {
  return decodeWith(vecU8, output, MAX_OUTPUT_BYTES);
}

// ---------------------------------------------------------------------------------------------
// Views

/** What the views need of the node's client: its read-only call (`KsanaRpcClient` has it). */
export interface StandardReader {
  call(request: CallRequest): Promise<Receipt>;
}

/** A contract refused a view: its revert reason, as the receipt gives it. */
export class StandardCallError extends Error {
  constructor(
    readonly method: string,
    readonly reason: string | null,
  ) {
    super(`${method}() failed: ${reason ?? 'unknown reason'}`);
    this.name = 'StandardCallError';
  }
}

async function view<T>(rpc: StandardReader, contract: Uint8Array, method: string, args: Uint8Array, def: TypeDef<T>): Promise<T> {
  const receipt = await rpc.call({ contract: assertBytes(contract, 32, 'contract'), method, args });
  if (!receipt.success) throw new StandardCallError(method, receipt.error);
  return decodeWith(def, receipt.output, MAX_OUTPUT_BYTES);
}

const NONE = new Uint8Array(0);
const OWNER_SPENDER = struct<{ owner: Uint8Array; spender: Uint8Array }>([
  ['owner', account],
  ['spender', account],
]);
const OWNER_OPERATOR = struct<{ owner: Uint8Array; operator: Uint8Array }>([
  ['owner', account],
  ['operator', account],
]);
const PAGE = struct<{ start: bigint; limit: number }>([
  ['start', u64],
  ['limit', u32],
]);
const OWNER_PAGE = struct<{ owner: Uint8Array; start: bigint; limit: number }>([
  ['owner', account],
  ['start', u64],
  ['limit', u32],
]);
const KS20_METADATA = struct<{ name: string; symbol: string; decimals: number }>([
  ['name', str],
  ['symbol', str],
  ['decimals', u8],
]);
const KS721_METADATA = struct<{ name: string; symbol: string }>([
  ['name', str],
  ['symbol', str],
]);
const ALLOWANCE_INFO = struct<{ amount: bigint; expires_at: bigint | null }>([
  ['amount', u128],
  ['expires_at', optU64],
]);
const OPERATOR_INFO = struct<{ approved: boolean; expires_at: bigint | null }>([
  ['approved', bool],
  ['expires_at', optU64],
]);

/**
 * A contract's declaration (`standards()`): the standard first, then its extensions; `null` when the contract refuses the view or
 * answers something else (a contract of no standard, or no contract at all). A failure of the node itself still throws.
 */
export async function readStandards(rpc: StandardReader, contract: Uint8Array): Promise<string[] | null> {
  try {
    return await view(rpc, contract, 'standards', NONE, strings);
  } catch (e) {
    if (e instanceof StandardCallError || e instanceof CodecError) return null;
    throw e;
  }
}

/** The capabilities a contract reports now (`capabilities()`), in `STANDARD_CAPABILITIES` order, then any of its own. */
export async function readCapabilities(rpc: StandardReader, contract: Uint8Array): Promise<string[]> {
  return view(rpc, contract, 'capabilities', NONE, strings);
}

/** What `readKs20` reads of a token. */
export interface Ks20Info {
  /** The declaration: `KS-20`, then the extensions. */
  standards: string[];
  name: string;
  symbol: string;
  /** At most `KS20_MAX_DECIMALS`. */
  decimals: number;
  total_supply: bigint;
  capabilities: string[];
}

/** A KS-20 token's declaration, metadata, total supply and capabilities; `null` when the contract declares no KS-20. */
export async function readKs20(rpc: StandardReader, token: Uint8Array): Promise<Ks20Info | null> {
  const standards = await readStandards(rpc, token);
  if (standards === null || standardOf(standards) !== KS20) return null;
  const metadata = await ks20Metadata(rpc, token);
  const total_supply = await ks20TotalSupply(rpc, token);
  const capabilities = await readCapabilities(rpc, token);
  return { standards, ...metadata, total_supply, capabilities };
}

/** KS-20 `metadata()`: the name, the symbol and the decimals. */
export async function ks20Metadata(rpc: StandardReader, token: Uint8Array): Promise<{ name: string; symbol: string; decimals: number }> {
  return view(rpc, token, 'metadata', NONE, KS20_METADATA);
}

/** KS-20 `total_supply()`. */
export async function ks20TotalSupply(rpc: StandardReader, token: Uint8Array): Promise<bigint> {
  return view(rpc, token, 'total_supply', NONE, u128);
}

/** KS-20 `balance_of(account)`. */
export async function ks20BalanceOf(rpc: StandardReader, token: Uint8Array, holder: Uint8Array): Promise<bigint> {
  return view(rpc, token, 'balance_of', encodeWith(account, holder), u128);
}

/** KS-20 `allowance(owner, spender)`: what `spender` may take now, 0 once the allowance expired. */
export async function ks20Allowance(rpc: StandardReader, token: Uint8Array, owner: Uint8Array, spender: Uint8Array): Promise<bigint> {
  return view(rpc, token, 'allowance', encodeWith(OWNER_SPENDER, { owner, spender }), u128);
}

/** KS-20 `allowance_info(owner, spender)`: the allowance as it was set, expired or not (see `isExpired`). */
export async function ks20AllowanceInfo(
  rpc: StandardReader,
  token: Uint8Array,
  owner: Uint8Array,
  spender: Uint8Array,
): Promise<{ amount: bigint; expires_at: bigint | null }> {
  return view(rpc, token, 'allowance_info', encodeWith(OWNER_SPENDER, { owner, spender }), ALLOWANCE_INFO);
}

/** `minter()` of a `KS-20-mintable` token: `null` once minting ended for good. */
export async function ks20Minter(rpc: StandardReader, token: Uint8Array): Promise<Uint8Array | null> {
  return view(rpc, token, 'minter', NONE, optAccount);
}

/** `cap()` of a `KS-20-mintable` token: the most the total supply can reach, `null` for no cap. */
export async function ks20Cap(rpc: StandardReader, token: Uint8Array): Promise<bigint | null> {
  return view(rpc, token, 'cap', NONE, option(u128));
}

/** What `readKs721` reads of a collection. */
export interface Ks721Info {
  /** The declaration: `KS-721`, then the extensions. */
  standards: string[];
  name: string;
  symbol: string;
  capabilities: string[];
  /** How many tokens exist, for a `KS-721-enumerable` collection; `null` for another. */
  total_supply: bigint | null;
}

/** A KS-721 collection's declaration, metadata and capabilities (and total supply when it is enumerable); `null` for no KS-721. */
export async function readKs721(rpc: StandardReader, collection: Uint8Array): Promise<Ks721Info | null> {
  const standards = await readStandards(rpc, collection);
  if (standards === null || standardOf(standards) !== KS721) return null;
  const metadata = await ks721Metadata(rpc, collection);
  const capabilities = await readCapabilities(rpc, collection);
  const total_supply = standards.includes(KS721_ENUMERABLE) ? await ks721TotalSupply(rpc, collection) : null;
  return { standards, ...metadata, capabilities, total_supply };
}

/** KS-721 `metadata()`: the name and the symbol. */
export async function ks721Metadata(rpc: StandardReader, collection: Uint8Array): Promise<{ name: string; symbol: string }> {
  return view(rpc, collection, 'metadata', NONE, KS721_METADATA);
}

/** KS-721 `owner_of(token_id)`: `null` for a token that does not exist, or was burned. */
export async function ks721OwnerOf(rpc: StandardReader, collection: Uint8Array, tokenId: Uint8Array): Promise<Uint8Array | null> {
  return view(rpc, collection, 'owner_of', encodeWith(account, tokenId), optAccount);
}

/** KS-721 `balance_of(owner)`: how many of the collection's tokens `owner` holds. */
export async function ks721BalanceOf(rpc: StandardReader, collection: Uint8Array, owner: Uint8Array): Promise<bigint> {
  return view(rpc, collection, 'balance_of', encodeWith(account, owner), u64);
}

/** KS-721 `token_uri(token_id)`: where the token's metadata is, `null` for none. */
export async function ks721TokenUri(rpc: StandardReader, collection: Uint8Array, tokenId: Uint8Array): Promise<string | null> {
  return view(rpc, collection, 'token_uri', encodeWith(account, tokenId), option(str));
}

/** KS-721 `approved(token_id)`: the account that may move the token besides its owner and the owner's operators. */
export async function ks721Approved(rpc: StandardReader, collection: Uint8Array, tokenId: Uint8Array): Promise<Uint8Array | null> {
  return view(rpc, collection, 'approved', encodeWith(account, tokenId), optAccount);
}

/** KS-721 `is_operator(owner, operator)`: whether `operator` may move all of `owner`'s tokens now (false once expired). */
export async function ks721IsOperator(
  rpc: StandardReader,
  collection: Uint8Array,
  owner: Uint8Array,
  operator: Uint8Array,
): Promise<boolean> {
  return view(rpc, collection, 'is_operator', encodeWith(OWNER_OPERATOR, { owner, operator }), bool);
}

/** KS-721 `operator_info(owner, operator)`: the operator as it was set, expired or not (see `isExpired`). */
export async function ks721OperatorInfo(
  rpc: StandardReader,
  collection: Uint8Array,
  owner: Uint8Array,
  operator: Uint8Array,
): Promise<{ approved: boolean; expires_at: bigint | null }> {
  return view(rpc, collection, 'operator_info', encodeWith(OWNER_OPERATOR, { owner, operator }), OPERATOR_INFO);
}

/** `total_supply()` of a `KS-721-enumerable` collection: how many tokens exist. */
export async function ks721TotalSupply(rpc: StandardReader, collection: Uint8Array): Promise<bigint> {
  return view(rpc, collection, 'total_supply', NONE, u64);
}

function pageLimit(limit: number): number {
  if (!Number.isInteger(limit) || limit < 1 || limit > KS721_MAX_PAGE) throw new RangeError(`a page holds 1 to ${KS721_MAX_PAGE} tokens`);
  return limit;
}

/** `tokens(start, limit)` of a `KS-721-enumerable` collection: a page of every token, from position `start`. */
export async function ks721Tokens(
  rpc: StandardReader,
  collection: Uint8Array,
  start: bigint,
  limit: number = KS721_MAX_PAGE,
): Promise<Uint8Array[]> {
  return view(rpc, collection, 'tokens', encodeWith(PAGE, { start, limit: pageLimit(limit) }), vec(account));
}

/** `tokens_of(owner, start, limit)` of a `KS-721-enumerable` collection: a page of `owner`'s tokens. */
export async function ks721TokensOf(
  rpc: StandardReader,
  collection: Uint8Array,
  owner: Uint8Array,
  start: bigint,
  limit: number = KS721_MAX_PAGE,
): Promise<Uint8Array[]> {
  return view(rpc, collection, 'tokens_of', encodeWith(OWNER_PAGE, { owner, start, limit: pageLimit(limit) }), vec(account));
}

// ---------------------------------------------------------------------------------------------
// Events

/** A KS-20 event. */
export type Ks20Event =
  /** The declaration, the first event of `init`. */
  | { kind: 'Standards'; ids: string[] }
  /** Emitted by `init` after the declaration, and again when the name or the symbol changes. */
  | { kind: 'Metadata'; name: string; symbol: string; decimals: number }
  /** Every change of a balance: a mint is from the zero account, a burn to it, and a fee is a transfer of its own. */
  | { kind: 'Transfer'; from: Uint8Array; to: Uint8Array; amount: bigint }
  /** Every `approve`: the allowance set, and its expiry (`null`: never). */
  | { kind: 'Approve'; owner: Uint8Array; spender: Uint8Array; amount: bigint; expires_at: bigint | null }
  /** `KS-20-mintable`: the minter set by `init` (from `null`) or by `set_minter`; `null` ends minting for good. */
  | { kind: 'Minter'; previous: Uint8Array | null; minter: Uint8Array | null };

/** A KS-721 event. */
export type Ks721Event =
  /** The declaration, the first event of `init`. */
  | { kind: 'Standards'; ids: string[] }
  /** Emitted by `init` after the declaration, and again when the name or the symbol changes. */
  | { kind: 'Metadata'; name: string; symbol: string }
  /** Every change of owner: a mint is from the zero account, a burn to it. */
  | { kind: 'Transfer'; from: Uint8Array; to: Uint8Array; token_id: Uint8Array }
  /** Every `approve`, and the clearing of a token's approval when it changes owner (`spender` `null`). */
  | { kind: 'Approve'; owner: Uint8Array; spender: Uint8Array | null; token_id: Uint8Array }
  /** Every `set_operator`. */
  | { kind: 'Operator'; owner: Uint8Array; operator: Uint8Array; approved: boolean; expires_at: bigint | null }
  /** A token's URI changed (`token_id`), or every token's (`null`). */
  | { kind: 'Uri'; token_id: Uint8Array | null };

/** An event of a contract that declared a standard. */
export type StandardEvent = { standard: typeof KS20; event: Ks20Event } | { standard: typeof KS721; event: Ks721Event };

type Shape = { readonly kind: string; readonly def: TypeDef<Record<string, unknown>> };

/** An event's layout: its name first (`event`), then its fields. */
function shape(kind: string, fields: ReadonlyArray<readonly [string, TypeDef<unknown>]>): Shape {
  return { kind, def: struct<Record<string, unknown>>([['event', str], ...fields]) };
}

const KS20_EVENTS: ReadonlyMap<string, Shape> = new Map([
  ['standards', shape('Standards', [['ids', strings]])],
  ['metadata', shape('Metadata', [['name', str], ['symbol', str], ['decimals', u8]])],
  ['transfer', shape('Transfer', [['from', account], ['to', account], ['amount', u128]])],
  ['approve', shape('Approve', [['owner', account], ['spender', account], ['amount', u128], ['expires_at', optU64]])],
  ['minter', shape('Minter', [['previous', optAccount], ['minter', optAccount]])],
]);

const KS721_EVENTS: ReadonlyMap<string, Shape> = new Map([
  ['standards', shape('Standards', [['ids', strings]])],
  ['metadata', shape('Metadata', [['name', str], ['symbol', str]])],
  ['transfer', shape('Transfer', [['from', account], ['to', account], ['token_id', account]])],
  ['approve', shape('Approve', [['owner', account], ['spender', optAccount], ['token_id', account]])],
  ['operator', shape('Operator', [['owner', account], ['operator', account], ['approved', bool], ['expires_at', optU64]])],
  ['uri', shape('Uri', [['token_id', optAccount]])],
]);

/** The name an event's bytes start with (a Borsh string), if it is short enough to be one of the standards'. */
function eventName(data: Uint8Array): string | null {
  if (data.length < 4) return null;
  const length = new DataView(data.buffer, data.byteOffset, 4).getUint32(0, true);
  if (length > MAX_EVENT_NAME_BYTES || data.length < 4 + length) return null;
  try {
    // `ignoreBOM`: a leading U+FEFF is part of the name, as in Rust's `String::from_utf8` (D-133); the default would drop it
    return new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(data.subarray(4, 4 + length));
  } catch {
    return null;
  }
}

function decodeEvent(events: ReadonlyMap<string, Shape>, data: Uint8Array): Record<string, unknown> | null {
  const bytes = assertBytes(data, undefined, 'event');
  const name = eventName(bytes);
  const found = name === null ? undefined : events.get(name);
  if (found === undefined) return null;
  let value: Record<string, unknown>;
  try {
    value = decodeWith(found.def, bytes, MAX_EVENT_BYTES);
  } catch (e) {
    if (e instanceof CodecError) return null;
    throw e;
  }
  const { event: _name, ...fields } = value;
  return { kind: found.kind, ...fields };
}

function encodeEvent(events: ReadonlyMap<string, Shape>, event: { kind: string }): Uint8Array {
  for (const [name, found] of events) {
    if (found.kind !== event.kind) continue;
    const { kind: _kind, ...fields } = event as Record<string, unknown>;
    return encodeWith(found.def, { event: name, ...fields });
  }
  throw new TypeError(`unknown event kind ${JSON.stringify(event.kind)}`);
}

/** Decodes a KS-20 event; `null` for bytes that are no KS-20 event (another name, other fields, bytes left over). */
export function decodeKs20Event(data: Uint8Array): Ks20Event | null {
  return decodeEvent(KS20_EVENTS, data) as Ks20Event | null;
}

/** Decodes a KS-721 event; `null` for bytes that are no KS-721 event. */
export function decodeKs721Event(data: Uint8Array): Ks721Event | null {
  return decodeEvent(KS721_EVENTS, data) as Ks721Event | null;
}

/** A KS-20 event's bytes, as the reference implementation emits it. */
export function encodeKs20Event(event: Ks20Event): Uint8Array {
  return encodeEvent(KS20_EVENTS, event);
}

/** A KS-721 event's bytes. */
export function encodeKs721Event(event: Ks721Event): Uint8Array {
  return encodeEvent(KS721_EVENTS, event);
}

/**
 * Decodes an event of a contract by the contract's declaration (`standards`, as `readStandards` reads it or an indexer recorded
 * it); `null` for a contract of no standard and for bytes that are no event of its standard. Only events after the declaration
 * count (`docs/STANDARDS.md` §2), which is the caller's to know.
 */
export function decodeStandardEvent(standards: readonly string[], data: Uint8Array): StandardEvent | null {
  const standard = standardOf(standards);
  if (standard === KS20) {
    const event = decodeKs20Event(data);
    return event === null ? null : { standard, event };
  }
  if (standard === KS721) {
    const event = decodeKs721Event(data);
    return event === null ? null : { standard, event };
  }
  return null;
}
