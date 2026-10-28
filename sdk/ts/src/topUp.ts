/**
 * Top-up (D-078, D-085): the buyer pays, with an ordinary BTC transaction, into a lock of the exchange contract (example
 * `contracts/examples/ksa-exchange`) and claims the KSA in the same transaction.
 *
 * The flow is "order → lock → confirm → redeem":
 *
 * 1. Order (off chain): buyer and seller agree on the quantity, the price (KSA base units per sat) and the buyer's Ksana account.
 * 2. Lock (seller, path B): the seller calls the contract's `lock` (arguments built with `lockArgs`; KSA may be attached), locking
 *    the KSA to the buyer's account and fixing the price, a BTC payment script used only for this lock, and the expiry BTC height.
 *    The seller cannot take back the locked KSA and the terms cannot be changed.
 * 3. Confirm (buyer, path C): the buyer reads the lock with `getLock` and builds the payment transaction with `buildTopUp`: the
 *    outputs are the KSN1 data output carrying the claim operation (`claim(lock)`), the amount paid to the payment script, and the
 *    change; the wallet signs it and it is broadcast.
 * 4. Redeem: after BTC confirmation the claim operation is force-included; the contract credits KSA, by the paid amount and the
 *    price, to the beneficiary account fixed at lock time; KSA the payment did not buy stays locked for the beneficiary (D-098).
 *    The beneficiary account then uses the KSA via path B.
 *
 * The claim operation is signed by a one-time key with nonce 0: a one-time account's nonce is always 0, so it does not consume the
 * wallet account's nonce, and the confirmation order of several top-ups does not matter. The one-time key signs only this one
 * VmOp and can be discarded afterwards; the wallet only needs to sign this BTC transaction (`SIGHASH_ALL`, no BIP322 message
 * signature). The claim operation carries only the lock id: the beneficiary account is fixed in the lock, so copying or
 * replaying the operation cannot change where the KSA goes.
 *
 * Copies and front-running: the signature of a path C operation covers the first input of the transaction carrying it (D-086),
 * so a copy of the claim operation in another transaction fails without consuming the one-time account's nonce, and the buyer's
 * own payment still claims. Anyone may claim a lock with an operation of their own in a transaction that pays its script, but the
 * KSA always goes to the beneficiary and a partial payment leaves the rest locked (D-098), so a seller who pays its own script a
 * few sats before the buyer's payment lands only gives the buyer KSA. After paying, check the lock (`getLock`) and the beneficiary's
 * balance rather than only the status of `opId`.
 *
 * Checks before paying (`buildTopUp`): the lock's beneficiary account must be the account the buyer gives (`beneficiary`,
 * otherwise the KSA is credited to someone else); the lock must be `Open` (paying into a claimed or released lock buys no KSA); by
 * default the payment must buy exactly all of the locked KSA: overpaying buys nothing more (the part above the `satsForAll` given
 * by `quoteTopUp` is wasted; `allowOverpay` permits it), underpaying buys only a part and leaves the rest locked until another
 * payment buys it or the seller releases it after the expiry (`allowPartial` permits it); `maxGas` cannot exceed `native_max_gas` (otherwise the operation is void and the
 * nonce consumed); too close to expiry it refuses (`currentBtcHeight + expiryMargin ≥ lock.expiry`; by default
 * `DEFAULT_EXPIRY_MARGIN` blocks are kept): `RELEASE_DELAY` BTC blocks after the lock's expiry the seller can release, and the
 * payment must get `NATIVE_CONFIRMATIONS` confirmations and be force-included before that, otherwise the claim fails while the BTC
 * is paid. `buildTopUp` gives the latest confirmation height `payBy`: when the payment transaction confirms only after BTC height
 * `lock.expiry + RELEASE_DELAY − native_confirmations`, the seller's release may execute first (the entries of parallel blocks
 * merged by a chain block execute before that block's force-included entries, D-063, hence another 1 is subtracted); if it does
 * not confirm for a long time, raise the fee rate with RBF.
 */

import { ByteReader, assertBytes, concatBytes, equalBytes, randomBytes, u32le, u64le } from './bytes.js';
import { type Params, VMOP_VERSION } from './params.js';
import { accountOf, isValidSecretKey } from './keys.js';
import { buildNativeTx, type NativeTxDraft, type Utxo, nativeFirstInput } from './pathC.js';
import { opId } from './hash.js';
import type { KsanaRpcClient } from './rpc.js';
import { signedVmop } from './sign.js';
import type { VmOp } from './types.js';

/** How many BTC blocks after the lock's expiry the seller can release (contract constant `RELEASE_DELAY`). */
export const RELEASE_DELAY = 6;

/** Default expiry safety margin of `buildTopUp` (BTC blocks): the current height plus it must not reach the lock's expiry height. */
export const DEFAULT_EXPIRY_MARGIN = 3;

/** Argument of the contract's `lock` method: the payment script is at most 64 bytes (contract constant `MAX_SCRIPT_BYTES`). */
export const MAX_LOCK_SCRIPT_BYTES = 64;

/**
 * State of a lock (contract `LockState`, Borsh: `u8` variant index; `Claimed` is followed by `paid_sats: u64` and
 * `ksa_paid: u128`).
 */
export type ExchangeLockState =
  | { kind: 'Open' }
  | { kind: 'Claimed'; paidSats: bigint; ksaPaid: bigint }
  | { kind: 'Released' };

/** A lock of the exchange contract (return value of the contract method `get_lock`, same Borsh field order). */
export interface ExchangeLock {
  /** Seller account. */
  seller: Uint8Array;
  /** Beneficiary account: the buyer account that receives the KSA at claim, fixed at lock time. */
  beneficiary: Uint8Array;
  /** BTC payment script, used only for this lock. */
  script: Uint8Array;
  /** Price: KSA base units per sat (u128). */
  price: bigint;
  /** KSA still locked (u128): the amount locked minus what claims have bought (0 once claimed, D-098). */
  ksa: bigint;
  /** Expiry BTC height (u32); `RELEASE_DELAY` blocks later the seller can release. */
  expiry: number;
  /** State. */
  state: ExchangeLockState;
}

const U128_MAX = (1n << 128n) - 1n;

function u128(reader: ByteReader): bigint {
  const bytes = reader.read(16);
  let value = 0n;
  for (let i = 15; i >= 0; i -= 1) value = (value << 8n) | BigInt(bytes[i]!);
  return value;
}

/** u128 little-endian. */
function u128le(value: bigint, what: string): Uint8Array {
  if (typeof value !== 'bigint' || value < 0n || value > U128_MAX) throw new RangeError(`${what} must be a bigint within u128 range`);
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i += 1) out[i] = Number((value >> BigInt(8 * i)) & 0xffn);
  return out;
}

/**
 * Decodes the return value of the contract method `get_lock` (the bytes must be consumed completely and the state's variant index
 * must be valid).
 */
export function decodeLock(bytes: Uint8Array): ExchangeLock {
  const reader = new ByteReader(assertBytes(bytes, undefined, 'lock'), 'lock');
  const seller = reader.read(32);
  const beneficiary = reader.read(32);
  const script = reader.read(reader.u32());
  const price = u128(reader);
  const ksa = u128(reader);
  const expiry = reader.u32();
  const tag = reader.u8();
  let state: ExchangeLockState;
  if (tag === 0) state = { kind: 'Open' };
  else if (tag === 1) state = { kind: 'Claimed', paidSats: reader.u64(), ksaPaid: u128(reader) };
  else if (tag === 2) state = { kind: 'Released' };
  else throw new Error(`unknown lock state variant ${tag}`);
  if (reader.remaining !== 0) throw new Error(`lock encoding has ${reader.remaining} trailing bytes`);
  return { seller, beneficiary, script, price, ksa, expiry, state };
}

/** The same Borsh encoding as the contract (inverse of `decodeLock`). */
export function encodeLock(lock: ExchangeLock): Uint8Array {
  let state: Uint8Array;
  if (lock.state.kind === 'Open') state = Uint8Array.of(0);
  else if (lock.state.kind === 'Claimed') {
    state = concatBytes(Uint8Array.of(1), u64le(lock.state.paidSats), u128le(lock.state.ksaPaid, 'claimed KSA'));
  } else state = Uint8Array.of(2);
  return concatBytes(
    assertBytes(lock.seller, 32, 'seller account'),
    assertBytes(lock.beneficiary, 32, 'beneficiary account'),
    u32le(lock.script.length),
    lock.script,
    u128le(lock.price, 'price'),
    u128le(lock.ksa, 'locked KSA'),
    u32le(lock.expiry),
    state,
  );
}

/** Arguments of the contract method `lock`. */
export interface LockArgs {
  /** KSA to lock (base units, u128, greater than 0); whatever the free balance does not cover must be attached to the call. */
  ksa: bigint;
  /** Price: KSA base units per sat (u128, greater than 0). */
  price: bigint;
  /** The seller's BTC payment script (1 to 64 bytes, not OP_RETURN, never used by any lock before). */
  script: Uint8Array;
  /** Beneficiary account: the buyer's Ksana account (32 bytes). */
  beneficiary: Uint8Array;
  /** Expiry BTC height (u32, greater than the BTC height anchored by the current block). */
  expiry: number;
}

/** Arguments of `lock(ksa, price, script, beneficiary, expiry)`: the Borsh encoding of `(u128, u128, Vec<u8>, [u8; 32], u32)`. */
export function lockArgs(args: LockArgs): Uint8Array {
  const script = assertBytes(args.script, undefined, 'payment script');
  if (script.length === 0 || script.length > MAX_LOCK_SCRIPT_BYTES) {
    throw new RangeError(`payment script must be 1 to ${MAX_LOCK_SCRIPT_BYTES} bytes`);
  }
  if (!Number.isInteger(args.expiry) || args.expiry < 0 || args.expiry > 0xffff_ffff) throw new RangeError('expiry height is out of u32 range');
  return concatBytes(
    u128le(args.ksa, 'locked KSA'),
    u128le(args.price, 'price'),
    u32le(script.length),
    script,
    assertBytes(args.beneficiary, 32, 'beneficiary account'),
    u32le(args.expiry),
  );
}

/** Argument of `claim(lock)`: the Borsh encoding of a `u64` (the lock id). */
export function claimArgs(lockId: bigint): Uint8Array {
  return u64le(lockId);
}

/** Reads a lock with the node's read-only call (`ksana_call`). */
export async function getLock(rpc: KsanaRpcClient, exchange: Uint8Array, lockId: bigint): Promise<ExchangeLock> {
  const receipt = await rpc.call({ contract: exchange, method: 'get_lock', args: u64le(lockId) });
  if (!receipt.success) throw new Error(`reading lock ${lockId} failed: ${receipt.error ?? 'unknown reason'}`);
  return decodeLock(receipt.output);
}

/**
 * Quote: the KSA bought by paying `sats` sats (same as the contract: `min(sats × price, lock.ksa)`, saturating multiplication), and
 * the minimum number of sats `satsForAll` needed to get all of the KSA still locked (`ceil(lock.ksa / price)`; paying more still
 * yields only `lock.ksa`).
 */
export function quoteTopUp(lock: ExchangeLock, sats: bigint): { ksa: bigint; satsForAll: bigint } {
  if (typeof sats !== 'bigint' || sats < 0n) throw new RangeError('sats must be a non-negative bigint');
  if (lock.price <= 0n) throw new RangeError('lock price must be positive');
  const product = sats * lock.price;
  const saturated = product > U128_MAX ? U128_MAX : product;
  return {
    ksa: saturated < lock.ksa ? saturated : lock.ksa,
    satsForAll: (lock.ksa + lock.price - 1n) / lock.price,
  };
}

/** Arguments of `buildTopUp`. */
export interface TopUpArgs {
  /** Exchange contract ID. */
  exchange: Uint8Array;
  /** Lock id. */
  lockId: bigint;
  /** The lock (read with `getLock`). */
  lock: ExchangeLock;
  /** The buyer's Ksana account (32 bytes): must match the lock's beneficiary account, otherwise the KSA goes to someone else. */
  beneficiary: Uint8Array;
  /** Amount paid to the payment script (sats). */
  sats: bigint;
  /**
   * The current BTC height, for the expiry check: the chain tip height when the wallet has the BTC tip, otherwise the BTC height
   * anchored by the node's latest block (`header.btc_anchor.height` of `rpc.getBlock('latest')`, not above the tip, so the check
   * is looser).
   */
  currentBtcHeight: number;
  /** Funding UTXOs (confirmed ones at the wallet address), selected in the given order. */
  utxos: readonly Utxo[];
  /** Locking script of the change output. */
  changeScript: Uint8Array;
  /** BTC fee rate (sat/vB). */
  feeRate: number;
  /**
   * Network parameters: the node's, from `KsanaRpcClient.getParams` (the claim operation is signed with its chain ID; signed
   * for another network it would be included and fail while the payment confirms, A2-11, D-106).
   */
  params: Params;
  /** One-time private key of the claim operation (32 bytes); generated randomly by default. */
  oneTimeKey?: Uint8Array;
  /** Gas limit of the claim operation; defaults to `NATIVE_MAX_GAS`. */
  maxGas?: bigint;
  /** Allow paying more than `satsForAll` (the overpaid part buys no more KSA); refused by default. */
  allowOverpay?: boolean;
  /**
   * Allow paying less than `satsForAll` (buys only `sats × price`; the rest stays locked until another payment buys it or the
   * seller releases it after the expiry); refused by default.
   */
  allowPartial?: boolean;
  /**
   * Expiry safety margin (BTC blocks), default `DEFAULT_EXPIRY_MARGIN`: refused when
   * `currentBtcHeight + expiryMargin ≥ lock.expiry`.
   */
  expiryMargin?: number;
}

/** Result of `buildTopUp`: the PSBT for the wallet to sign and the claim operation. */
export interface TopUpDraft extends NativeTxDraft {
  /** The claim operation (path C, signed by the one-time key, nonce 0). */
  op: VmOp;
  /** The claim operation's `op_id` (track it with `ksana_getOpStatus`). */
  opId: Uint8Array;
  /** KSA bought at the lock's price (at most the locked KSA). */
  ksa: bigint;
  /**
   * Latest BTC height at which the payment transaction should confirm (`lock.expiry + RELEASE_DELAY − native_confirmations − 1`);
   * any later and the seller's release may execute first.
   */
  payBy: number;
}

/**
 * Builds the top-up transaction (unsigned PSBT): output 0 is the KSN1 data output carrying the claim operation, output 1 the
 * amount paid to the lock's payment script, then the change. Refuses when the beneficiary account is not `beneficiary`, the lock is
 * not `Open`, fewer than `expiryMargin` blocks remain before expiry, `maxGas` exceeds `native_max_gas`, or the payment differs from
 * `satsForAll` (overpaying without `allowOverpay`, underpaying without `allowPartial`).
 */
export function buildTopUp(args: TopUpArgs): TopUpDraft {
  const params = args.params;
  if (params === undefined) throw new TypeError("params is required: the parameters of the node's network (KsanaRpcClient.getParams)");
  if (typeof args.sats !== 'bigint' || args.sats <= 0n) throw new RangeError('payment amount must be a positive bigint (sats)');
  if (typeof args.lockId !== 'bigint' || args.lockId < 0n) throw new RangeError('lock id must be a non-negative bigint');
  if (!Number.isInteger(args.currentBtcHeight) || args.currentBtcHeight < 0) throw new RangeError('current BTC height must be a non-negative integer');
  const margin = args.expiryMargin ?? DEFAULT_EXPIRY_MARGIN;
  if (!Number.isInteger(margin) || margin < 0) throw new RangeError('expiry safety margin must be a non-negative integer');
  const { lock } = args;
  const beneficiary = assertBytes(args.beneficiary, 32, 'beneficiary account');
  if (!equalBytes(lock.beneficiary, beneficiary)) {
    throw new Error(`the beneficiary account of lock ${args.lockId} is not the given buyer account: the payment would credit the KSA to someone else`);
  }
  const maxGas = args.maxGas ?? params.native_max_gas;
  if (typeof maxGas !== 'bigint' || maxGas <= 0n || maxGas > params.native_max_gas) {
    throw new RangeError(`the claim operation's gas limit must be between 1 and native_max_gas (${params.native_max_gas})`);
  }
  if (lock.state.kind !== 'Open') {
    throw new Error(`lock ${args.lockId} is not Open (${lock.state.kind}): the payment would buy no KSA`);
  }
  if (args.currentBtcHeight + margin >= lock.expiry) {
    throw new Error(
      `lock ${args.lockId} expires at BTC height ${lock.expiry} (current ${args.currentBtcHeight}, safety margin ${margin} blocks): ` +
        `the payment may not be included before the seller releases`,
    );
  }
  const { ksa, satsForAll } = quoteTopUp(lock, args.sats);
  if (args.sats > satsForAll && args.allowOverpay !== true) {
    throw new Error(
      `paying ${args.sats} sats exceeds the ${satsForAll} sats needed for all ${lock.ksa} KSA base units: the overpaid part buys ` +
        `nothing more (pass allowOverpay if this is intended)`,
    );
  }
  if (args.sats < satsForAll && args.allowPartial !== true) {
    throw new Error(
      `paying ${args.sats} sats is an underpayment: all ${lock.ksa} KSA base units cost ${satsForAll} sats; underpaying buys only ` +
        `${ksa}; the rest stays locked until another payment buys it or the seller releases it (pass allowPartial if this is intended)`,
    );
  }
  const oneTimeKey = args.oneTimeKey ?? randomBytes(32);
  if (!isValidSecretKey(oneTimeKey)) throw new Error('one-time private key is invalid');
  const first = args.utxos[0];
  if (first === undefined) throw new Error('insufficient funds: no UTXO available');
  // The claim operation's signature is bound to the payment transaction's first input (D-086): the seller cannot copy it into a
  // transaction of their own to claim fraudulently
  const op = signedVmop(
    {
      version: VMOP_VERSION,
      chain_id: params.chain_id,
      account: accountOf(oneTimeKey),
      nonce: 0n,
      fee: { kind: 'BtcNative' },
      max_gas: maxGas,
      action: {
        kind: 'Call',
        contract: assertBytes(args.exchange, 32, 'exchange contract ID'),
        method: 'claim',
        args: claimArgs(args.lockId),
        funds: [],
      },
    },
    oneTimeKey,
    undefined,
    nativeFirstInput(first),
  );
  const draft = buildNativeTx({
    op,
    utxos: args.utxos,
    changeScript: args.changeScript,
    feeRate: args.feeRate,
    params,
    payments: [{ value: args.sats, script: lock.script }],
  });
  const payBy = lock.expiry + RELEASE_DELAY - params.native_confirmations - 1;
  return { ...draft, op, opId: opId(op), ksa, payBy };
}
