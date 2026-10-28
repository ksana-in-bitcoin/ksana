/**
 * Consensus data types (SPEC §4.1, §6.1, §7.1), corresponding field by field to `crates/ksana-primitives/src/types.rs`.
 *
 * - Field names are the same as in Rust (serde); the order of fields and variants is the Borsh encoding format (see
 *   `codec.ts`).
 * - Enums are represented as unions tagged with `kind`, where `kind` is the Rust variant name; the content of a tuple
 *   variant goes in the `bytes` field.
 * - `u8` / `u32` use `number`, `u64` / `u128` use `bigint`, byte strings use `Uint8Array`.
 * - All BTC transaction IDs and block hashes are stored in **Bitcoin internal byte order** (the raw sha256d output), the
 *   reverse of the hex shown by bitcoind RPC and block explorers (D-001); convert with `txidFromRpc` / `txidToRpc`.
 */

/** Reference to a Bitcoin transaction output (D-001). */
export interface OutPoint {
  /** Transaction ID (32 bytes, internal byte order). */
  txid: Uint8Array;
  /** Output index (u32). */
  vout: number;
}

/** Fee mode, corresponding to the two submission paths (SPEC §5; path A is deprecated, D-078). */
export type FeeMode =
  /** Path B: sponsored, paying in the project token (u128). */
  | { kind: 'Token'; amount: bigint }
  /** Path C: native transaction. */
  | { kind: 'BtcNative' };

/**
 * One asset attached to a call (D-073): transferred from the signing account to the contract before execution; the
 * contract reads it with `call_funds`. The attached funds of one action are strictly ascending by asset ID (no
 * duplicates), each amount greater than 0, at most `MAX_CALL_FUNDS` items (see `checkFunds`).
 */
export interface Funds {
  /** Asset ID (32 bytes; KSA is 32 zero bytes). */
  asset: Uint8Array;
  /** Amount (u128). */
  amount: bigint;
}

/** The action a VmOp performs (SPEC §4.1). */
export type Action =
  /** Deploy a contract: `funds` are transferred to the new contract before `init` runs. */
  | { kind: 'Deploy'; code: Uint8Array; init_args: Uint8Array; funds: Funds[] }
  /** Call a contract: `funds` are transferred to the contract before the call. */
  | { kind: 'Call'; contract: Uint8Array; method: string; args: Uint8Array; funds: Funds[] }
  /** Transfer a VM built-in asset (`amount` is u128). */
  | { kind: 'AssetTransfer'; asset: Uint8Array; to: Uint8Array; amount: bigint };

/** Signature scheme (SPEC §4.1, §4.2). */
export type SigScheme =
  /** BIP340 Schnorr signature (64 bytes). */
  | { kind: 'Bip340'; bytes: Uint8Array }
  /** BIP322 simple signature: the consensus encoding of the witness stack of the `to_sign` input (D-052). */
  | { kind: 'Bip322Simple'; bytes: Uint8Array };

/** A VM operation signed by the user (SPEC §4.1). */
export interface VmOp {
  /** Currently 2 (u8; version 2 `Deploy` / `Call` carry attached funds, D-073). */
  version: number;
  /** Chain ID (u32). */
  chain_id: number;
  /** Account: x-only public key (32 bytes). */
  account: Uint8Array;
  /** Strictly increasing from 0 within the account (u64). */
  nonce: bigint;
  /** Fee mode. */
  fee: FeeMode;
  /** Execution limit (u64). */
  max_gas: bigint;
  /** The action to perform. */
  action: Action;
  /** Signature; not part of the sighash. */
  sig: SigScheme;
}

/** The part of a VmOp other than `sig`: the content of the sighash (SPEC §4.2). */
export type UnsignedVmOp = Omit<VmOp, 'sig'>;

/** The BTC block a block anchors to (SPEC §6.1). */
export interface BtcAnchor {
  /** BTC height (u32). */
  height: number;
  /** BTC block hash (internal byte order). */
  hash: Uint8Array;
}

/** Fast-lane block header v2 (SPEC §6.1, D-063); the Borsh encoding is 168 + 32 × number of parents bytes. */
export interface BlockHeader {
  /** Block version (u32, currently 2). */
  version: number;
  /** Selected-chain depth (u64): the height of the selected parent plus 1; 0 for the genesis block. */
  height: bigint;
  /** Parent hashes: the first is the selected parent, the rest in ascending byte order; empty for the genesis block. */
  parents: Uint8Array[];
  /** The latest BTC block seen when the block was produced. */
  btc_anchor: BtcAnchor;
  /** Merkle root of all entries of this block. */
  entries_root: Uint8Array;
  /** State root after execution. */
  state_root: Uint8Array;
  /** Producer x-only public key. */
  producer: Uint8Array;
  /** Seconds (u64). */
  timestamp: bigint;
  /** Difficulty target (u32). */
  bits: number;
  /** PoW nonce (u64). */
  nonce: bigint;
}

/** Block entry (SPEC §6.1). */
export type Entry =
  /** Path C: a force-included native transaction. */
  | { kind: 'Forced'; btc_txid: Uint8Array; op: VmOp }
  /** Path B: sponsored. */
  | { kind: 'Sponsored'; op: VmOp }
  /**
   * Unlock sponsored fees and block rewards (§8.5, D-074, D-079): cumulative unlock in acceptance order up to this position:
   * the height `height` of the chain block that accepted it and its index `index` in that chain block's acceptance order
   * (the merged blocks are 0, 1, … in turn, the chain block itself comes last).
   */
  | { kind: 'Unlock'; height: bigint; index: number };

/** Entry in a settlement payload (D-003): one-to-one with `Entry`; `Forced` only writes `btc_txid`. */
export type EntryData =
  | { kind: 'Forced'; btc_txid: Uint8Array }
  | { kind: 'Sponsored'; op: VmOp }
  | { kind: 'Unlock'; height: bigint; index: number };

/** The settlement transactions carried by a block (D-081): the commit transaction and the reveal transaction, both in Bitcoin network serialization. The payload is in the witness of the reveal transaction (KSN1 envelope). */
export interface SettlementTxs {
  /** Commit transaction: the producer's funding inputs; the outputs are the change and the P2TR output committing to the envelope. */
  commit: Uint8Array;
  /** Reveal transaction: spends the commitment output and carries the payload in its witness; the outputs are a P2A and an empty OP_RETURN. */
  reveal: Uint8Array;
}

/** Fast-lane block (SPEC §6.1). */
export interface Block {
  /** Block header. */
  header: BlockHeader;
  /** All entries. */
  entries: Entry[];
  /** The signed settlement transactions (commit and reveal, D-081); `null` when there are none. */
  settlement: SettlementTxs | null;
}

/** The body of one block carried by a payload (D-077): all entries (in payload form) of the block that header number `header` of the same payload refers to. */
export interface Body {
  /** Index of the header in the header sequence of the same payload. */
  header: number;
  /** All entries of that block; `Forced` entries only write `btc_txid`. */
  entries: EntryData[];
}

/** The Borsh payload written to OP_RETURN (SPEC §7.1). */
export type Payload =
  /** Settlement transaction payload: the last header is this block, `entries` are this block's entries, `bodies` are the bodies of other blocks (D-077). */
  | { kind: 'Settlement'; headers: BlockHeader[]; entries: EntryData[]; bodies: Body[] }
  /** Payload of a path C native transaction. */
  | { kind: 'NativeOp'; op: VmOp }
  /** Republished headers and bodies. */
  | { kind: 'Headers'; headers: BlockHeader[]; bodies: Body[]; payee: Uint8Array };

/** The `EntryData` of a settlement payload from an `Entry` (`Forced` keeps only `btc_txid`). */
export function entryData(entry: Entry): EntryData {
  if (entry.kind === 'Forced') {
    return { kind: 'Forced', btc_txid: entry.btc_txid };
  }
  return entry;
}
