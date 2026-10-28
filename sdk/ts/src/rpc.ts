/**
 * JSON-RPC client for the node and the indexer (SPEC §11.2, `docs/RPC.md`, D-054), based on `fetch` (HTTP POST).
 *
 * - Parameters are passed by position (like the jsonrpsee client); byte strings are lowercase hex, `u64` / `u128` are decimal
 *   strings.
 * - Results are converted to SDK types (`bigint`, `Uint8Array`; block headers and entries are parsed strictly per the D-007 JSON
 *   representation). Result objects reject unknown fields just like the node's result types (serde's `deny_unknown_fields`): a
 *   version mismatch between the node and the SDK interface fails early.
 * - A JSON-RPC error throws `RpcError` (with the error code, see `RPC_ERROR_CODES`); an HTTP or response-format error throws
 *   `RpcTransportError`.
 */

import { fromHex, toHex } from './bytes.js';
import { encodeVmOp, fromJson, toJson } from './codec.js';
import { parseCanonicalDecimal } from './schema.js';
import { type Params, paramsForChainId } from './params.js';
import { KS20, KS721, type Standard } from './standards.js';
import type { BlockHeader, Entry, Funds, SettlementTxs, VmOp } from './types.js';

/** Error codes used by the node and the indexer (`docs/RPC.md`). */
export const RPC_ERROR_CODES = Object.freeze({
  /** Malformed parameter (hex, decoding, length). */
  INVALID_PARAMS: -32602,
  /** Invalid operation (violates consensus rules). */
  OP_INVALID: -32010,
  /** Operation rejected by this node's policy (fee too low, pool full, replacement fee not high enough). */
  OP_REJECTED: -32011,
  /** The queried object does not exist. */
  NOT_FOUND: -32004,
  /**
   * The node has no such method: an older version (for example one without the operation listings of D-111 or the standards
   * index of D-120), or one whose operation index and standards index are not built, which answers their methods so until a
   * later start builds them.
   */
  METHOD_NOT_FOUND: -32601,
  /** Internal node error. */
  INTERNAL: -32603,
});

/** A JSON-RPC error response. */
export class RpcError extends Error {
  constructor(
    /** Error code. */
    readonly code: number,
    message: string,
    /** Data attached to the error. */
    readonly data?: unknown,
    /** The method called. */
    readonly method?: string,
  ) {
    super(message);
    this.name = 'RpcError';
  }
}

/** Transport error: the HTTP status is not 2xx, the response is not JSON-RPC, or the result does not have the expected shape. */
export class RpcTransportError extends Error {
  constructor(
    message: string,
    /** HTTP status code (when there was a response). */
    readonly status?: number,
  ) {
    super(message);
    this.name = 'RpcTransportError';
  }
}

/** Client options. */
export interface RpcClientOptions {
  /** Replaces the global `fetch` (tests or a custom transport). */
  fetch?: typeof fetch;
  /** Extra HTTP headers. */
  headers?: Record<string, string>;
  /** Timeout of a single request, including response-body reads (milliseconds); no timeout by default. */
  timeoutMs?: number;
}

/** Generic JSON-RPC 2.0 client (HTTP POST). */
export class JsonRpcClient {
  private nextId = 1;
  private readonly fetchFn: typeof fetch;

  constructor(
    /** Server URL, e.g. `http://127.0.0.1:8545`. */
    readonly url: string,
    private readonly options: RpcClientOptions = {},
  ) {
    const fetchFn = options.fetch ?? globalThis.fetch;
    if (typeof fetchFn !== 'function') throw new Error('fetch is not available in this environment; provide it via options.fetch');
    this.fetchFn = fetchFn;
  }

  /** Calls `method` with positional parameters; returns `result`. */
  async request(method: string, params: readonly unknown[] = []): Promise<unknown> {
    const id = this.nextId++;
    const body = JSON.stringify({ jsonrpc: '2.0', id, method, params });
    const controller = this.options.timeoutMs === undefined ? undefined : new AbortController();
    const timer =
      controller === undefined ? undefined : setTimeout(() => controller.abort(), this.options.timeoutMs);
    // Not called with the client as `this`: the browser's fetch requires `this` to be the global object (or absent), otherwise
    // it throws Illegal invocation
    const fetchFn = this.fetchFn;
    let response: Response | undefined;
    let payload: unknown;
    try {
      response = await fetchFn(this.url, {
        method: 'POST',
        headers: { 'content-type': 'application/json', ...this.options.headers },
        body,
        ...(controller === undefined ? {} : { signal: controller.signal }),
      });
      // fetch resolves at the headers; keep the same deadline while the body is still arriving.
      payload = await response.json();
    } catch (e) {
      // A request that failed, or whose deadline passed while its body arrived, carries no status, as before: callers tell a
      // server's answer from a failed request by it (the web page's rpcErrorMayPass)
      if (response === undefined || controller?.signal.aborted) {
        throw new RpcTransportError(`${method}: request failed: ${e instanceof Error ? e.message : String(e)}`);
      }
      throw new RpcTransportError(`${method}: HTTP ${response.status}, response is not JSON`, response.status);
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
    if (typeof payload !== 'object' || payload === null) {
      throw new RpcTransportError(`${method}: response is not a JSON-RPC object`, response.status);
    }
    const reply = payload as { error?: unknown; result?: unknown };
    if (reply.error !== undefined && reply.error !== null) {
      const error = reply.error as { code?: unknown; message?: unknown; data?: unknown };
      const code = typeof error.code === 'number' ? error.code : RPC_ERROR_CODES.INTERNAL;
      const message = typeof error.message === 'string' ? error.message : JSON.stringify(reply.error);
      throw new RpcError(code, `${method}: ${message}`, error.data, method);
    }
    if (!response.ok) {
      throw new RpcTransportError(`${method}: HTTP ${response.status}`, response.status);
    }
    if (!('result' in reply)) throw new RpcTransportError(`${method}: response is missing result`, response.status);
    return reply.result;
  }
}

// ---------------------------------------------------------------------------------------------
// Result types and parsing

/** Height and hash of a block. */
export interface BlockRef {
  height: bigint;
  hash: Uint8Array;
}

/** An event. */
export interface RpcEvent {
  contract: Uint8Array;
  data: Uint8Array;
}

/** Receipt of an entry (`gas_used` is `max_gas` on failure). */
export interface Receipt {
  /** Operation ID (`null` for unlock entries and read-only calls). */
  op_id: Uint8Array | null;
  success: boolean;
  error: string | null;
  gas_used: bigint;
  output: Uint8Array;
  events: RpcEvent[];
  /**
   * The changes the op made to built-in assets, in the order it made them (D-122; empty on failure); `null` when the node did not
   * record them: a node from before them, or a receipt it stored before it recorded them.
   */
  asset_changes: AssetChange[] | null;
}

/** The kinds of [`AssetChange`], in the order the node defines them (D-122). */
export const ASSET_CHANGE_KINDS = ['transfer', 'funds', 'payout', 'issue', 'burn', 'create', 'freeze', 'allow', 'pause', 'renounce'] as const;

/**
 * A change an op made to a built-in asset (D-122): a movement or a management action of D-121. The movements: `transfer`, the
 * `AssetTransfer` action from its signer; `funds`, funds attached to a call or a deployment (or to `ksana_call`) by its signer, or by a
 * contract to the contract it calls or creates; `payout`, a contract's `asset_transfer`; `issue`, by the issuing contract (`asset_issue`,
 * `asset_create`); `burn`. The management actions: `create` (the capabilities the asset declared; its issuance follows as an `issue`
 * unless it is of 0), `freeze`, `allow`, `pause` and `renounce` (the capabilities given up, none when the asset had none of them; giving
 * up `pause` resumes the asset). KSA's fees, unlocks and
 * rewards are not among them, nor is a movement of 0. `events` is how many of the op's events came before it.
 */
export type AssetChange =
  | { kind: 'transfer' | 'funds' | 'payout'; asset: Uint8Array; from: Uint8Array; to: Uint8Array; amount: bigint; events: number }
  | { kind: 'issue'; asset: Uint8Array; to: Uint8Array; amount: bigint; events: number }
  | { kind: 'burn'; asset: Uint8Array; from: Uint8Array; amount: bigint; events: number }
  | { kind: 'create' | 'renounce'; asset: Uint8Array; capabilities: AssetCapability[]; events: number }
  | { kind: 'freeze'; asset: Uint8Array; account: Uint8Array; frozen: boolean; events: number }
  | { kind: 'allow'; asset: Uint8Array; account: Uint8Array; allowed: boolean; events: number }
  | { kind: 'pause'; asset: Uint8Array; paused: boolean; events: number };

/**
 * Operation status (`docs/RPC.md`). `skipped`: only included by blocks that the main chain merged and could not be executed at
 * merge time (duplicate, nonce conflict, etc.); neither executed nor charged (D-063).
 */
export type OpState = 'pending' | 'fast_confirmed' | 'btc_final' | 'failed' | 'skipped';

/** Result of `ksana_getOpStatus`. */
export interface OpStatus {
  status: OpState;
  block: BlockRef | null;
  index: number | null;
  receipt: Receipt | null;
}

/**
 * A position on the main chain (D-111): the height of the main-chain block that accepted a block, the block's place in that
 * main-chain block's acceptance order (the blocks it merges first, in consensus order, the main-chain block itself last) and the
 * index of the entry in the block. Positions sort in the order the main chain executes the entries; in JSON it is the string
 * `height:slot:entry` (`formatOpPosition`, `parseOpPosition`).
 */
export interface OpPosition {
  height: bigint;
  slot: number;
  entry: number;
}

/** One operation of a listing (`ksana_getAccountOps`, `ksana_getLatestOps`, D-111). */
export interface OpRef {
  op_id: Uint8Array;
  /** Its status on the main chain, as `ksana_getOpStatus` gives it there (never `pending`). */
  status: Exclude<OpState, 'pending'>;
  /** The block that includes it (`hash`) and the height of the main-chain block that accepted that block (`height`). */
  block: BlockRef;
  /** Index among the block's entries. */
  index: number;
  /** Its position on the main chain: passed as `before`, a listing continues below it. */
  position: OpPosition;
}

/** A page of a listing of the operations on the main chain, newest first (D-111). */
export interface OpList {
  ops: OpRef[];
  /** Where the next page starts (pass it as `before`); `null` when nothing is left below this page. */
  next: OpPosition | null;
  /** The main-chain tip the page is based on. */
  at: BlockRef;
}

/** Options of `getAccountOps` and `getLatestOps`. */
export interface OpListOptions {
  /** List the operations below this position (an earlier page's `next`); from the newest when omitted. */
  before?: OpPosition | string | null;
  /** At most this many operations, 1 to `MAX_OPS_LIMIT`; the node's default (`DEFAULT_OPS_LIMIT`) when omitted. */
  limit?: number;
}

/** The number of operations in a page of `getAccountOps` / `getLatestOps` when no `limit` is given (D-111). */
export const DEFAULT_OPS_LIMIT = 50;

/** The most operations a page of `getAccountOps` / `getLatestOps` holds (D-111). */
export const MAX_OPS_LIMIT = 500;

/**
 * Options of the listings of the node's index of the standards' events (D-120): `getAccountAssets`, `getTokenHolders`, `getNfts`
 * and `getApprovals`.
 */
export interface IndexPageOptions {
  /** Continue after an earlier page: its `next`; from the first item when omitted. */
  after?: string | null;
  /** At most this many items, 1 to `MAX_OPS_LIMIT`; the node's default (`DEFAULT_OPS_LIMIT`) when omitted. */
  limit?: number;
}

/** Result of `ksana_getContract` (D-120). */
export interface ContractInfo {
  contract: Uint8Array;
  /** The SHA-256 of its code. */
  code_hash: Uint8Array;
  deployer: Uint8Array;
  /**
   * The IDs of the standards it declared (`docs/STANDARDS.md` §2), the standard's first; empty when it declared none, or a
   * declaration that is not 1 to 16 IDs of 1 to 64 bytes.
   */
  standards: string[];
  /** The position of the op whose events hold its declaration; `null` when it made none. */
  declared: OpPosition | null;
  at: BlockRef;
}

/**
 * One asset an account holds (`ksana_getAccountAssets`, D-120): a built-in asset (`asset`, KSA is 32 zero bytes), a KS-20 token
 * (`token`, its balance by the token's events) or a KS-721 collection (`nft`, how many of its NFTs the account holds).
 */
export type AccountAsset =
  | { kind: 'asset'; asset: Uint8Array; balance: bigint }
  | { kind: 'token'; contract: Uint8Array; balance: bigint }
  | { kind: 'nft'; contract: Uint8Array; count: bigint };

/** A page of `ksana_getAccountAssets`: the built-in assets by asset ID, then the tokens and collections by contract ID. */
export interface AccountAssets {
  assets: AccountAsset[];
  /** Where the next page starts (pass it as `after`); `null` once the listing reached its end. */
  next: string | null;
  at: BlockRef;
}

/** Result of `ksana_getToken` (D-120): what the node's index knows of a KS-20 token or a KS-721 collection. */
export interface TokenInfo {
  contract: Uint8Array;
  standard: Standard;
  /** The IDs of the extensions it declared. */
  extensions: string[];
  /** From its latest `metadata` event; `null` before one. */
  name: string | null;
  symbol: string | null;
  /** A token's decimals; `null` for a collection, or before a `metadata` event. */
  decimals: number | null;
  /** A token's supply (the sum of its balances), or how many NFTs of a collection exist, by its events. */
  supply: bigint;
  /** How many accounts hold some. */
  holders: bigint;
  /** The position of the op whose events hold its declaration. */
  declared: OpPosition;
  /**
   * Whether the index passed over an event of the standard it emitted (a transfer of more than the sender held by its events, of an
   * NFT `from` did not own, a second declaration, metadata out of bounds): its balances may then differ from what it answers itself.
   */
  irregular: boolean;
  at: BlockRef;
}

/** One holder of a token, or of a collection's NFTs (`ksana_getTokenHolders`, D-120). */
export interface TokenHolder {
  account: Uint8Array;
  /** Its balance, or the number of the collection's NFTs it holds. */
  balance: bigint;
}

/** A page of `ksana_getTokenHolders`: the largest balance first. */
export interface TokenHolders {
  holders: TokenHolder[];
  /** Where the next page starts (pass it as `after`); `null` once the listing reached its end. */
  next: string | null;
  at: BlockRef;
}

/** One NFT (`ksana_getNfts`, D-120). */
export interface NftRef {
  contract: Uint8Array;
  token_id: Uint8Array;
}

/** A page of `ksana_getNfts`: by collection, then token ID. */
export interface NftList {
  nfts: NftRef[];
  /** Where the next page starts (pass it as `after`); `null` once the listing reached its end. */
  next: string | null;
  at: BlockRef;
}

/** Options of `getNfts`. */
export interface NftListOptions extends IndexPageOptions {
  /** Only the NFTs of this collection. */
  contract?: Uint8Array | string | null;
}

/**
 * One approval an account gave (`ksana_getApprovals`, D-120), as its latest event set it, expired or not (`expires_at`: the block
 * timestamp from which it no longer counts, `null` never): a KS-20 allowance (what is left of it is the token's `allowance` view), a
 * KS-721 operator, or the approved account of one NFT.
 */
export type ApprovalRef =
  | { kind: 'allowance'; contract: Uint8Array; spender: Uint8Array; amount: bigint; expires_at: bigint | null }
  | { kind: 'operator'; contract: Uint8Array; operator: Uint8Array; expires_at: bigint | null }
  | { kind: 'token'; contract: Uint8Array; token_id: Uint8Array; spender: Uint8Array };

/** A page of `ksana_getApprovals`: by contract, then kind (allowance, operator, token), then spender, operator or token ID. */
export interface ApprovalList {
  approvals: ApprovalRef[];
  /** Where the next page starts (pass it as `after`); `null` once the listing reached its end. */
  next: string | null;
  at: BlockRef;
}

/** Result of `ksana_getBlock`. */
export interface BlockInfo {
  hash: Uint8Array;
  header: BlockHeader;
  entries: Entry[];
  /** The settlement transactions carried by the block (commit and reveal, D-081). */
  settlement: SettlementTxs | null;
  receipts: Receipt[];
  /** Height of the chain block that accepted it (D-063); `null` while the main chain has not accepted it yet. */
  accepted_height: bigint | null;
  /** The blocks it merges (the merge set without the selected parent, in consensus order, D-063). */
  merged: Uint8Array[];
}

/** An asset balance. */
export interface Balance {
  asset: Uint8Array;
  amount: bigint;
}

/**
 * Income of a producer not yet unlocked (§8.5, D-074, D-079): for one block produced by the account after the unlock cursor, the
 * path B sponsored fees plus the block reward that will be paid out at unlock; no longer listed once unlocked, when the amount moves
 * into the available balance.
 */
export interface LockedFee {
  block_hash: Uint8Array;
  amount: bigint;
}

/** The capabilities an asset's issuing contract can hold, in the order the node lists them (D-121). */
export const ASSET_CAPABILITIES = ['mint', 'freeze', 'allowlist', 'pause'] as const;
/**
 * A capability of an asset's issuing contract (D-121): `mint` issues more, `freeze` stops an account's movements, `allowlist` admits
 * only the accounts it allows, `pause` stops every movement but its own.
 */
export type AssetCapability = (typeof ASSET_CAPABILITIES)[number];

/** The status flags an account can have for an asset, in the order the node lists them (D-121). */
export const ASSET_STATUS_FLAGS = ['frozen', 'allowed'] as const;
/** A status flag of an account for an asset (D-121); it counts only while the asset has the matching capability. */
export type AssetStatusFlag = (typeof ASSET_STATUS_FLAGS)[number];

/** Result of `ksana_getAsset` (D-074, D-121). */
export interface AssetInfo {
  /** Asset ID (32 zero bytes for KSA). */
  asset: Uint8Array;
  name: string;
  /** Symbol, 1 to 16 bytes (D-121). */
  symbol: string;
  decimals: number;
  /** Total supply (base units); for KSA only the block rewards already paid out (unlocked) count (D-079). */
  total_supply: bigint;
  /** Issuing contract (32 zero bytes for KSA). */
  issuer: Uint8Array;
  /**
   * The capabilities the issuing contract still holds (D-121); empty for KSA and for an asset whose supply is fixed and whose
   * movements no one can stop. They can only be given up, never gained.
   */
  capabilities: AssetCapability[];
  /** Whether the issuing contract paused the asset: then no account but the issuing contract sends it (D-121). */
  paused: boolean;
  at: BlockRef;
}

/** Result of `ksana_getAssetStatus` (D-121): an account's standing in an asset. */
export interface AssetStatus {
  asset: Uint8Array;
  account: Uint8Array;
  /** The account's status flags as recorded; a flag counts only while the asset has the matching capability. */
  status: AssetStatusFlag[];
  /** Whether the asset's rules let the account send or burn it now (its balance aside). */
  can_send: boolean;
  /** Whether the asset's rules let the account receive it now. */
  can_receive: boolean;
  at: BlockRef;
}

/** Result of `ksana_getChainInfo` (D-076): the main-chain tip and the protocol version. */
export interface ChainInfo {
  chain_id: number;
  tip: BlockRef;
  /** DAA score of the tip (D-063). */
  daa_score: bigint;
  /** Protocol version of the tip (block header version). */
  protocol_version: number;
  /** The next scheduled upgrade after the tip; `null` when there is none. */
  next_upgrade: { version: number; daa_score: bigint } | null;
  /** The highest protocol version the node software supports. */
  max_known_version: number;
  /**
   * The BTC-final tip (D-082, D-114): the highest block of the finalized chain determined at a BTC block that has
   * `BTC_FINALITY_CONFS` confirmations; only a BTC reorg reaching that BTC block can take it back. The genesis block when there
   * is none yet.
   */
  finalized: BlockRef;
}

/** Result of `ksana_getAccount`. */
export interface AccountInfo {
  account: Uint8Array;
  /** The next usable nonce. */
  nonce: bigint;
  /** Available balance per asset (assets with a zero balance are omitted). */
  balances: Balance[];
  /** Sponsored fees and block rewards not yet unlocked as a producer (D-074). */
  locked_fees: LockedFee[];
  at: BlockRef;
}

/** Request of `ksana_call`. */
export interface CallRequest {
  /** Contract ID (32 bytes or its hex). */
  contract: Uint8Array | string;
  method: string;
  args: Uint8Array;
  /** Attached funds (D-073), transferred from `caller` to the contract; empty by default. */
  funds?: Funds[];
  /** The simulated caller (32 bytes or its hex); defaults to 32 zero bytes. */
  caller?: Uint8Array | string;
  /** Gas limit; defaults to `BLOCK_GAS_LIMIT`. */
  max_gas?: bigint;
}

/** BTC status of the settlement transaction. */
export type SettlementState = 'pending' | 'broadcast' | 'confirmed';

/** Result of `ksana_getSettlement`. */
export interface SettlementInfo {
  block: BlockRef;
  /**
   * ID (internal byte order) of the settlement's reveal transaction (the one carrying the payload, D-081): the settlement
   * transaction carried by the block; for a block without a settlement transaction, the one carried by the block that carries its
   * body (`carrier`); `null` while no block carries it yet (D-077).
   */
  txid: Uint8Array | null;
  /** The same ID in RPC display byte order. */
  txid_rpc: string | null;
  /** ID of the commit transaction (internal byte order). */
  commit_txid: Uint8Array | null;
  /** The same ID in RPC display byte order. */
  commit_txid_rpc: string | null;
  /** The commit and reveal transactions (network serialization). */
  txs: SettlementTxs | null;
  /**
   * Carrier (D-095): the payee account of the sender of the payload that first put this block's data on chain (the producer of the
   * containing block for a settlement payload, its `payee` for a republished payload); at unlock it receives `CARRIER_BPS` of each
   * of the block's reward slots and `CARRIER_FEE_BPS` of the path B sponsored fees; `null` when there is none (the reward goes to the
   * producer). Nodes before M23 do not provide it.
   */
  carrier_account: Uint8Array | null;
  /**
   * For a block without a settlement transaction, the block whose settlement payload carries its body (D-077); `null` for a block
   * that carries a settlement transaction.
   */
  carrier: BlockRef | null;
  status: SettlementState;
  /**
   * The confirmed transaction that put the block's data on chain: the earliest confirmed among the transactions whose payload
   * carries its body (D-077).
   */
  confirmed: { txid: Uint8Array; height: number; block: Uint8Array; confirmations: number } | null;
  settled: boolean;
  btc_final: boolean;
}

/** Result of `ksana_estimateFee` (D-054). */
export interface FeeEstimate {
  /** Fee rate of the settlement transaction (sat/vB). */
  feerate_sat_vb: bigint;
  /**
   * Path B: `amount` is the suggested token amount (KSA base units) for a plain transfer; `per_kib` is the suggested fee rate, the
   * token amount per KiB of cost, where cost = max(VmOp encoded bytes, `max_gas × MAX_BLOCK_DATA_BYTES / BLOCK_GAS_LIMIT`); other
   * operations scale by their own cost (nodes before M23 do not provide it; recorded as 0).
   */
  path_b: { amount: bigint; per_kib: bigint };
  /** The block the estimate is based on (the tip). */
  at: BlockRef;
}

/**
 * Cost of an operation in the producer's packing budget (same as the node's local policy): the larger of the VmOp encoded bytes and
 * the bytes `max_gas` converts to (a block's gas limit corresponds to a block's data limit), at least 1. `feeFor` uses it with
 * `FeeEstimate.path_b.per_kib` to estimate the suggested token amount of an operation.
 */
export function opCost(params: Params, encodedLen: number, maxGas: bigint): bigint {
  const bytes = BigInt(encodedLen);
  const byGas = params.block_gas_limit === 0n ? 0n : (maxGas * BigInt(params.max_block_data_bytes)) / params.block_gas_limit;
  const cost = bytes > byGas ? bytes : byGas;
  return cost < 1n ? 1n : cost;
}

/**
 * Token amount of an operation with cost `cost` at the suggested rate `perKib` (per KiB of cost), rounded up and not below `floor`.
 */
export function feeFor(perKib: bigint, cost: bigint, floor = 0n): bigint {
  const amount = (perKib * cost + 1023n) / 1024n;
  return amount > floor ? amount : floor;
}

/** Summary of a new block pushed by `ksana_subscribeBlocks`. */
export interface BlockSummary {
  height: bigint;
  hash: Uint8Array;
  entries: number;
  timestamp: bigint;
}

/** Result of `indexer_status`. */
export interface IndexerStatus {
  height: bigint;
  hash: Uint8Array;
  state_root: Uint8Array;
  btc_height: number | null;
  /** The BTC-final tip (D-082, D-114), by the same rule as `finalized` of `ksana_getChainInfo`. */
  finalized: BlockRef;
  halted: string | null;
}

/** Result of `indexer_stateRoot`. */
export interface StateRootInfo {
  height: bigint;
  hash: Uint8Array;
  state_root: Uint8Array;
}

/** Block identifier: `'latest'`, a height (`bigint` / integer / decimal string) or a block hash (32 bytes or 64 hex digits). */
export type BlockId = 'latest' | bigint | number | Uint8Array | string;

/** Block identifier → RPC parameter string. */
export function blockIdParam(id: BlockId): string {
  if (id instanceof Uint8Array) {
    if (id.length !== 32) throw new Error('block hash must be 32 bytes');
    return toHex(id);
  }
  if (typeof id === 'bigint') {
    if (id < 0n) throw new RangeError('height cannot be negative');
    return id.toString();
  }
  if (typeof id === 'number') {
    if (!Number.isSafeInteger(id) || id < 0) throw new RangeError('height must be a non-negative integer');
    return String(id);
  }
  if (id === 'latest' || /^(0|[1-9][0-9]*)$/.test(id) || /^[0-9a-fA-F]{64}$/.test(id)) return id.toLowerCase();
  throw new Error('block identifier must be "latest", a decimal height or a 64-digit hex hash');
}

const U64_MAX = (1n << 64n) - 1n;
const U32_MAX = 0xffff_ffff;

/** Parses a position `height:slot:entry` (decimal numbers without leading zeros, D-111). */
export function parseOpPosition(text: string): OpPosition {
  const m = /^(0|[1-9][0-9]*):(0|[1-9][0-9]*):(0|[1-9][0-9]*)$/.exec(text);
  if (m === null) throw new Error(`a position must be height:slot:entry in decimal, such as 81:0:2, not ${JSON.stringify(text)}`);
  return checkPosition({ height: BigInt(m[1] ?? ''), slot: Number(m[2]), entry: Number(m[3]) });
}

/** The string form `height:slot:entry` of a position (D-111). */
export function formatOpPosition(position: OpPosition): string {
  const p = checkPosition(position);
  return `${p.height}:${p.slot}:${p.entry}`;
}

function checkPosition(p: OpPosition): OpPosition {
  const u32 = (n: number) => Number.isInteger(n) && n >= 0 && n <= U32_MAX;
  if (typeof p.height !== 'bigint' || p.height < 0n || p.height > U64_MAX || !u32(p.slot) || !u32(p.entry)) {
    throw new Error('a position is a u64 height and u32 slot and entry');
  }
  return p;
}

function listParams(options: OpListOptions): [string | null, number | null] {
  const { before, limit } = options;
  if (limit !== undefined && !(Number.isInteger(limit) && limit >= 1 && limit <= MAX_OPS_LIMIT)) {
    throw new Error(`limit must be an integer from 1 to ${MAX_OPS_LIMIT}`);
  }
  const position = before === undefined || before === null ? null : typeof before === 'string' ? parseOpPosition(before) : before;
  return [position === null ? null : formatOpPosition(position), limit ?? null];
}

/** The cursors of the listings of the standards index (D-120), by their lengths in bytes. */
const ASSET_CURSOR_BYTES = 33;
const HOLDER_CURSOR_BYTES = 48;
const NFT_CURSOR_BYTES = 64;
const APPROVAL_CURSOR_BYTES = 65;

function pageParams(options: IndexPageOptions, cursorBytes: number): [string | null, number | null] {
  const { after, limit } = options;
  if (limit !== undefined && !(Number.isInteger(limit) && limit >= 1 && limit <= MAX_OPS_LIMIT)) {
    throw new Error(`limit must be an integer from 1 to ${MAX_OPS_LIMIT}`);
  }
  if (after !== undefined && after !== null && !new RegExp(`^[0-9a-fA-F]{${2 * cursorBytes}}$`).test(after)) {
    throw new Error(`after must be ${cursorBytes} bytes of hex, a page's next`);
  }
  return [after === undefined || after === null ? null : after.toLowerCase(), limit ?? null];
}

function bytesParam(value: Uint8Array | string, length: number, what: string): string {
  const bytes = typeof value === 'string' ? fromHex(value, length, what) : value;
  if (!(bytes instanceof Uint8Array) || bytes.length !== length) throw new Error(`${what} must be ${length} bytes`);
  return toHex(bytes);
}

type Obj = Record<string, unknown>;

function bad(what: string, detail: string): never {
  throw new RpcTransportError(`malformed result: ${what} ${detail}`);
}

function obj(value: unknown, what: string): Obj {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) bad(what, 'must be an object');
  return value as Obj;
}

/** A result object: only the fields in `keys` are allowed, like the node's result types (serde's `deny_unknown_fields`). */
function record(value: unknown, what: string, keys: readonly string[]): Obj {
  const o = obj(value, what);
  for (const key of Object.keys(o)) {
    if (!keys.includes(key)) bad(what, `has unknown field ${JSON.stringify(key)}`);
  }
  return o;
}

function arr(value: unknown, what: string): unknown[] {
  if (!Array.isArray(value)) bad(what, 'must be an array');
  return value;
}

function hexOf(value: unknown, what: string, length?: number): Uint8Array {
  if (typeof value !== 'string') bad(what, 'must be a hex string');
  try {
    return fromHex(value, length, what);
  } catch (e) {
    return bad(what, e instanceof Error ? e.message : String(e));
  }
}

/** u64 / u128: a canonical decimal string (D-007). */
function decOf(value: unknown, what: string): bigint {
  if (typeof value !== 'string') bad(what, 'must be a decimal string');
  try {
    return parseCanonicalDecimal(value, what);
  } catch (e) {
    return bad(what, e instanceof Error ? e.message : String(e));
  }
}

function numOf(value: unknown, what: string): number {
  if (typeof value !== 'number' || !Number.isInteger(value) || value < 0) bad(what, 'must be a non-negative integer');
  return value;
}

function strOf(value: unknown, what: string): string {
  if (typeof value !== 'string') bad(what, 'must be a string');
  return value;
}

/** A cursor of the standards index: hex of `length` bytes, kept as the node wrote it. */
function cursorOf(value: unknown, what: string, length: number): string {
  hexOf(value, what, length);
  return value as string;
}

function boolOf(value: unknown, what: string): boolean {
  if (typeof value !== 'boolean') bad(what, 'must be a boolean');
  return value;
}

function nullable<T>(value: unknown, parse: (v: unknown) => T): T | null {
  return value === null || value === undefined ? null : parse(value);
}

function parseBlockRef(value: unknown, what: string): BlockRef {
  const o = record(value, what, ['height', 'hash']);
  return { height: decOf(o.height, `${what}.height`), hash: hexOf(o.hash, `${what}.hash`, 32) };
}

/** Parses a receipt; a node from before D-122 gives no `asset_changes`, read as `null`. */
export function parseReceipt(value: unknown, what = 'receipt'): Receipt {
  const o = record(value, what, ['op_id', 'success', 'error', 'gas_used', 'output', 'events', 'asset_changes']);
  return {
    op_id: nullable(o.op_id, (v) => hexOf(v, `${what}.op_id`, 32)),
    success: boolOf(o.success, `${what}.success`),
    error: nullable(o.error, (v) => (typeof v === 'string' ? v : bad(`${what}.error`, 'must be a string'))),
    gas_used: decOf(o.gas_used, `${what}.gas_used`),
    output: hexOf(o.output, `${what}.output`),
    events: arr(o.events, `${what}.events`).map((e, i) => {
      const ev = record(e, `${what}.events[${i}]`, ['contract', 'data']);
      return { contract: hexOf(ev.contract, `${what}.events[${i}].contract`, 32), data: hexOf(ev.data, `${what}.events[${i}].data`) };
    }),
    asset_changes: nullable(o.asset_changes, (v) =>
      arr(v, `${what}.asset_changes`).map((c, i) => parseAssetChange(c, `${what}.asset_changes[${i}]`)),
    ),
  };
}

/** Parses one of a receipt's asset changes (D-122). */
function parseAssetChange(value: unknown, what: string): AssetChange {
  const kind = oneOf(obj(value, what).kind, ASSET_CHANGE_KINDS, `${what}.kind`);
  const fields = (...keys: string[]): Obj => record(value, what, ['kind', 'asset', ...keys, 'events']);
  const key = (o: Obj, name: string): Uint8Array => hexOf(o[name], `${what}.${name}`, 32);
  const head = (o: Obj) => ({ asset: key(o, 'asset'), events: numOf(o.events, `${what}.events`) });
  const amount = (o: Obj): bigint => decOf(o.amount, `${what}.amount`);
  switch (kind) {
    case 'transfer':
    case 'funds':
    case 'payout': {
      const o = fields('from', 'to', 'amount');
      return { kind, ...head(o), from: key(o, 'from'), to: key(o, 'to'), amount: amount(o) };
    }
    case 'issue': {
      const o = fields('to', 'amount');
      return { kind, ...head(o), to: key(o, 'to'), amount: amount(o) };
    }
    case 'burn': {
      const o = fields('from', 'amount');
      return { kind, ...head(o), from: key(o, 'from'), amount: amount(o) };
    }
    case 'create':
    case 'renounce': {
      const o = fields('capabilities');
      return { kind, ...head(o), capabilities: namesOf(o.capabilities, ASSET_CAPABILITIES, `${what}.capabilities`) };
    }
    case 'freeze': {
      const o = fields('account', 'frozen');
      return { kind, ...head(o), account: key(o, 'account'), frozen: boolOf(o.frozen, `${what}.frozen`) };
    }
    case 'allow': {
      const o = fields('account', 'allowed');
      return { kind, ...head(o), account: key(o, 'account'), allowed: boolOf(o.allowed, `${what}.allowed`) };
    }
    case 'pause': {
      const o = fields('paused');
      return { kind, ...head(o), paused: boolOf(o.paused, `${what}.paused`) };
    }
  }
}

function oneOf<T extends string>(value: unknown, allowed: readonly T[], what: string): T {
  if (typeof value !== 'string' || !(allowed as readonly string[]).includes(value)) bad(what, `must be one of ${allowed.join(' / ')}`);
  return value as T;
}

function protocol<T>(parse: () => T, what: string): T {
  try {
    return parse();
  } catch (e) {
    return bad(what, e instanceof Error ? e.message : String(e));
  }
}

/** Parses the result of `ksana_getOpStatus`. */
export function parseOpStatus(value: unknown): OpStatus {
  const o = record(value, 'op status', ['status', 'block', 'index', 'receipt']);
  return {
    status: oneOf(o.status, ['pending', 'fast_confirmed', 'btc_final', 'failed', 'skipped'] as const, 'status'),
    block: nullable(o.block, (v) => parseBlockRef(v, 'block')),
    index: nullable(o.index, (v) => numOf(v, 'index')),
    receipt: nullable(o.receipt, (v) => parseReceipt(v)),
  };
}

function positionOf(value: unknown, what: string): OpPosition {
  if (typeof value !== 'string') bad(what, 'must be a position string');
  try {
    return parseOpPosition(value);
  } catch (e) {
    return bad(what, e instanceof Error ? e.message : String(e));
  }
}

/** Parses the result of `ksana_getAccountOps` and `ksana_getLatestOps`. */
export function parseOpList(value: unknown): OpList {
  const o = record(value, 'op list', ['ops', 'next', 'at']);
  return {
    ops: arr(o.ops, 'ops').map((v, i) => {
      const x = record(v, `ops[${i}]`, ['op_id', 'status', 'block', 'index', 'position']);
      return {
        op_id: hexOf(x.op_id, `ops[${i}].op_id`, 32),
        status: oneOf(x.status, ['fast_confirmed', 'btc_final', 'failed', 'skipped'] as const, `ops[${i}].status`),
        block: parseBlockRef(x.block, `ops[${i}].block`),
        index: numOf(x.index, `ops[${i}].index`),
        position: positionOf(x.position, `ops[${i}].position`),
      };
    }),
    next: nullable(o.next, (v) => positionOf(v, 'next')),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getContract` (D-120). */
export function parseContractInfo(value: unknown): ContractInfo {
  const o = record(value, 'contract', ['contract', 'code_hash', 'deployer', 'standards', 'declared', 'at']);
  return {
    contract: hexOf(o.contract, 'contract', 32),
    code_hash: hexOf(o.code_hash, 'code_hash', 32),
    deployer: hexOf(o.deployer, 'deployer', 32),
    standards: arr(o.standards, 'standards').map((v, i) => strOf(v, `standards[${i}]`)),
    declared: nullable(o.declared, (v) => positionOf(v, 'declared')),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getAccountAssets` (D-120). */
export function parseAccountAssets(value: unknown): AccountAssets {
  const o = record(value, 'account assets', ['assets', 'next', 'at']);
  return {
    assets: arr(o.assets, 'assets').map((v, i): AccountAsset => {
      const what = `assets[${i}]`;
      const kind = oneOf(obj(v, what).kind, ['asset', 'token', 'nft'] as const, `${what}.kind`);
      if (kind === 'asset') {
        const x = record(v, what, ['kind', 'asset', 'balance']);
        return { kind, asset: hexOf(x.asset, `${what}.asset`, 32), balance: decOf(x.balance, `${what}.balance`) };
      }
      if (kind === 'token') {
        const x = record(v, what, ['kind', 'contract', 'balance']);
        return { kind, contract: hexOf(x.contract, `${what}.contract`, 32), balance: decOf(x.balance, `${what}.balance`) };
      }
      const x = record(v, what, ['kind', 'contract', 'count']);
      return { kind, contract: hexOf(x.contract, `${what}.contract`, 32), count: decOf(x.count, `${what}.count`) };
    }),
    next: nullable(o.next, (v) => cursorOf(v, 'next', ASSET_CURSOR_BYTES)),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getToken` (D-120). */
export function parseTokenInfo(value: unknown): TokenInfo {
  const o = record(value, 'token', [
    'contract',
    'standard',
    'extensions',
    'name',
    'symbol',
    'decimals',
    'supply',
    'holders',
    'declared',
    'irregular',
    'at',
  ]);
  const decimals = nullable(o.decimals, (v) => numOf(v, 'decimals'));
  if (decimals !== null && decimals > 255) bad('decimals', 'exceeds u8');
  return {
    contract: hexOf(o.contract, 'contract', 32),
    standard: oneOf(o.standard, [KS20, KS721] as const, 'standard'),
    extensions: arr(o.extensions, 'extensions').map((v, i) => strOf(v, `extensions[${i}]`)),
    name: nullable(o.name, (v) => strOf(v, 'name')),
    symbol: nullable(o.symbol, (v) => strOf(v, 'symbol')),
    decimals,
    supply: decOf(o.supply, 'supply'),
    holders: decOf(o.holders, 'holders'),
    declared: positionOf(o.declared, 'declared'),
    irregular: boolOf(o.irregular, 'irregular'),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getTokenHolders` (D-120). */
export function parseTokenHolders(value: unknown): TokenHolders {
  const o = record(value, 'token holders', ['holders', 'next', 'at']);
  return {
    holders: arr(o.holders, 'holders').map((v, i) => {
      const x = record(v, `holders[${i}]`, ['account', 'balance']);
      return { account: hexOf(x.account, `holders[${i}].account`, 32), balance: decOf(x.balance, `holders[${i}].balance`) };
    }),
    next: nullable(o.next, (v) => cursorOf(v, 'next', HOLDER_CURSOR_BYTES)),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getNfts` (D-120). */
export function parseNftList(value: unknown): NftList {
  const o = record(value, 'nfts', ['nfts', 'next', 'at']);
  return {
    nfts: arr(o.nfts, 'nfts').map((v, i) => {
      const x = record(v, `nfts[${i}]`, ['contract', 'token_id']);
      return { contract: hexOf(x.contract, `nfts[${i}].contract`, 32), token_id: hexOf(x.token_id, `nfts[${i}].token_id`, 32) };
    }),
    next: nullable(o.next, (v) => cursorOf(v, 'next', NFT_CURSOR_BYTES)),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getApprovals` (D-120). */
export function parseApprovalList(value: unknown): ApprovalList {
  const o = record(value, 'approvals', ['approvals', 'next', 'at']);
  return {
    approvals: arr(o.approvals, 'approvals').map((v, i): ApprovalRef => {
      const what = `approvals[${i}]`;
      const kind = oneOf(obj(v, what).kind, ['allowance', 'operator', 'token'] as const, `${what}.kind`);
      const expiry = (x: Obj) => nullable(x.expires_at, (e) => decOf(e, `${what}.expires_at`));
      if (kind === 'allowance') {
        const x = record(v, what, ['kind', 'contract', 'spender', 'amount', 'expires_at']);
        return {
          kind,
          contract: hexOf(x.contract, `${what}.contract`, 32),
          spender: hexOf(x.spender, `${what}.spender`, 32),
          amount: decOf(x.amount, `${what}.amount`),
          expires_at: expiry(x),
        };
      }
      if (kind === 'operator') {
        const x = record(v, what, ['kind', 'contract', 'operator', 'expires_at']);
        return {
          kind,
          contract: hexOf(x.contract, `${what}.contract`, 32),
          operator: hexOf(x.operator, `${what}.operator`, 32),
          expires_at: expiry(x),
        };
      }
      const x = record(v, what, ['kind', 'contract', 'token_id', 'spender']);
      return {
        kind,
        contract: hexOf(x.contract, `${what}.contract`, 32),
        token_id: hexOf(x.token_id, `${what}.token_id`, 32),
        spender: hexOf(x.spender, `${what}.spender`, 32),
      };
    }),
    next: nullable(o.next, (v) => cursorOf(v, 'next', APPROVAL_CURSOR_BYTES)),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getBlock`. */
export function parseBlockInfo(value: unknown): BlockInfo {
  const o = record(value, 'block', ['hash', 'header', 'entries', 'settlement', 'receipts', 'accepted_height', 'merged']);
  return {
    hash: hexOf(o.hash, 'hash', 32),
    header: protocol(() => fromJson('BlockHeader', o.header), 'header'),
    entries: arr(o.entries, 'entries').map((e, i) => protocol(() => fromJson('Entry', e), `entries[${i}]`)),
    settlement: nullable(o.settlement, (v) => parseSettlementTxs(v, 'settlement')),
    receipts: arr(o.receipts, 'receipts').map((r, i) => parseReceipt(r, `receipts[${i}]`)),
    accepted_height: nullable(o.accepted_height, (v) => decOf(v, 'accepted_height')),
    merged: arr(o.merged, 'merged').map((h, i) => hexOf(h, `merged[${i}]`, 32)),
  };
}

/** Parses the result of `ksana_getAccount`. */
export function parseAccountInfo(value: unknown): AccountInfo {
  const o = record(value, 'account', ['account', 'nonce', 'balances', 'locked_fees', 'at']);
  return {
    account: hexOf(o.account, 'account', 32),
    nonce: decOf(o.nonce, 'nonce'),
    balances: arr(o.balances, 'balances').map((b, i) => {
      const x = record(b, `balances[${i}]`, ['asset', 'amount']);
      return { asset: hexOf(x.asset, `balances[${i}].asset`, 32), amount: decOf(x.amount, `balances[${i}].amount`) };
    }),
    locked_fees: arr(o.locked_fees, 'locked_fees').map((f, i) => {
      const x = record(f, `locked_fees[${i}]`, ['block_hash', 'amount']);
      return {
        block_hash: hexOf(x.block_hash, `locked_fees[${i}].block_hash`, 32),
        amount: decOf(x.amount, `locked_fees[${i}].amount`),
      };
    }),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** A list of names from `allowed`, each at most once and in the order of `allowed`. */
function namesOf<T extends string>(value: unknown, allowed: readonly T[], what: string): T[] {
  const names = arr(value, what).map((v, i) => oneOf(v, allowed, `${what}[${i}]`));
  names.forEach((name, i) => {
    if (i > 0 && allowed.indexOf(name) <= allowed.indexOf(names[i - 1] as T)) bad(what, 'must list each name once, in order');
  });
  return names;
}

/** Parses the result of `ksana_getAsset`. */
export function parseAssetInfo(value: unknown): AssetInfo {
  const o = record(value, 'asset', ['asset', 'name', 'symbol', 'decimals', 'total_supply', 'issuer', 'capabilities', 'paused', 'at']);
  if (typeof o.name !== 'string') bad('name', 'must be a string');
  if (typeof o.symbol !== 'string') bad('symbol', 'must be a string');
  const decimals = numOf(o.decimals, 'decimals');
  if (decimals > 255) bad('decimals', 'exceeds u8');
  return {
    asset: hexOf(o.asset, 'asset', 32),
    name: o.name,
    symbol: o.symbol,
    decimals,
    total_supply: decOf(o.total_supply, 'total_supply'),
    issuer: hexOf(o.issuer, 'issuer', 32),
    capabilities: namesOf(o.capabilities, ASSET_CAPABILITIES, 'capabilities'),
    paused: boolOf(o.paused, 'paused'),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getAssetStatus`. */
export function parseAssetStatus(value: unknown): AssetStatus {
  const o = record(value, 'asset_status', ['asset', 'account', 'status', 'can_send', 'can_receive', 'at']);
  return {
    asset: hexOf(o.asset, 'asset', 32),
    account: hexOf(o.account, 'account', 32),
    status: namesOf(o.status, ASSET_STATUS_FLAGS, 'status'),
    can_send: boolOf(o.can_send, 'can_send'),
    can_receive: boolOf(o.can_receive, 'can_receive'),
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses the result of `ksana_getChainInfo`. */
export function parseChainInfo(value: unknown): ChainInfo {
  const o = record(value, 'chain_info', ['chain_id', 'tip', 'daa_score', 'protocol_version', 'next_upgrade', 'max_known_version', 'finalized']);
  const next = o.next_upgrade;
  let nextUpgrade: ChainInfo['next_upgrade'] = null;
  if (next !== null) {
    const u = record(next, 'next_upgrade', ['version', 'daa_score']);
    nextUpgrade = { version: numOf(u.version, 'next_upgrade.version'), daa_score: decOf(u.daa_score, 'next_upgrade.daa_score') };
  }
  return {
    chain_id: numOf(o.chain_id, 'chain_id'),
    tip: parseBlockRef(o.tip, 'tip'),
    daa_score: decOf(o.daa_score, 'daa_score'),
    protocol_version: numOf(o.protocol_version, 'protocol_version'),
    next_upgrade: nextUpgrade,
    max_known_version: numOf(o.max_known_version, 'max_known_version'),
    finalized: parseBlockRef(o.finalized, 'finalized'),
  };
}

/** The settlement transactions `{commit, reveal}` of a block (D-081). */
function parseSettlementTxs(value: unknown, what: string): SettlementTxs {
  const o = record(value, what, ['commit', 'reveal']);
  return { commit: hexOf(o.commit, `${what}.commit`), reveal: hexOf(o.reveal, `${what}.reveal`) };
}

/** Parses the result of `ksana_getSettlement`. */
export function parseSettlementInfo(value: unknown): SettlementInfo {
  const o = record(value, 'settlement', [
    'block',
    'txid',
    'txid_rpc',
    'commit_txid',
    'commit_txid_rpc',
    'txs',
    'carrier_account',
    'carrier',
    'status',
    'confirmed',
    'settled',
    'btc_final',
  ]);
  const text = (v: unknown, field: string) => (typeof v === 'string' ? v : bad(field, 'must be a string'));
  return {
    block: parseBlockRef(o.block, 'block'),
    txid: nullable(o.txid, (v) => hexOf(v, 'txid', 32)),
    txid_rpc: nullable(o.txid_rpc, (v) => text(v, 'txid_rpc')),
    commit_txid: nullable(o.commit_txid, (v) => hexOf(v, 'commit_txid', 32)),
    commit_txid_rpc: nullable(o.commit_txid_rpc, (v) => text(v, 'commit_txid_rpc')),
    txs: nullable(o.txs, (v) => parseSettlementTxs(v, 'txs')),
    carrier_account: o.carrier_account === undefined ? null : nullable(o.carrier_account, (v) => hexOf(v, 'carrier_account', 32)),
    carrier: nullable(o.carrier, (v) => parseBlockRef(v, 'carrier')),
    status: oneOf(o.status, ['pending', 'broadcast', 'confirmed'] as const, 'status'),
    confirmed: nullable(o.confirmed, (v) => {
      const c = record(v, 'confirmed', ['txid', 'height', 'block', 'confirmations']);
      return {
        txid: hexOf(c.txid, 'confirmed.txid', 32),
        height: numOf(c.height, 'confirmed.height'),
        block: hexOf(c.block, 'confirmed.block', 32),
        confirmations: numOf(c.confirmations, 'confirmed.confirmations'),
      };
    }),
    settled: boolOf(o.settled, 'settled'),
    btc_final: boolOf(o.btc_final, 'btc_final'),
  };
}

/** Parses the result of `ksana_estimateFee`. */
export function parseFeeEstimate(value: unknown): FeeEstimate {
  const o = record(value, 'fee estimate', ['feerate_sat_vb', 'path_b', 'at']);
  const b = record(o.path_b, 'path_b', ['amount', 'per_kib']);
  return {
    feerate_sat_vb: decOf(o.feerate_sat_vb, 'feerate_sat_vb'),
    path_b: { amount: decOf(b.amount, 'path_b.amount'), per_kib: b.per_kib === undefined ? 0n : decOf(b.per_kib, 'path_b.per_kib') },
    at: parseBlockRef(o.at, 'at'),
  };
}

/** Parses a block summary pushed by `ksana_subscribeBlocks`. */
export function parseBlockSummary(value: unknown): BlockSummary {
  const o = record(value, 'block summary', ['height', 'hash', 'entries', 'timestamp']);
  return {
    height: decOf(o.height, 'height'),
    hash: hexOf(o.hash, 'hash', 32),
    entries: numOf(o.entries, 'entries'),
    timestamp: decOf(o.timestamp, 'timestamp'),
  };
}

/** Parses the result of `indexer_status`. */
export function parseIndexerStatus(value: unknown): IndexerStatus {
  const o = record(value, 'indexer status', ['height', 'hash', 'state_root', 'btc_height', 'finalized', 'halted']);
  return {
    height: decOf(o.height, 'height'),
    hash: hexOf(o.hash, 'hash', 32),
    state_root: hexOf(o.state_root, 'state_root', 32),
    btc_height: nullable(o.btc_height, (v) => numOf(v, 'btc_height')),
    finalized: parseBlockRef(o.finalized, 'finalized'),
    halted: nullable(o.halted, (v) => (typeof v === 'string' ? v : bad('halted', 'must be a string'))),
  };
}

/** Parses the result of `indexer_stateRoot`. */
export function parseStateRootInfo(value: unknown): StateRootInfo {
  const o = record(value, 'state root', ['height', 'hash', 'state_root']);
  return {
    height: decOf(o.height, 'height'),
    hash: hexOf(o.hash, 'hash', 32),
    state_root: hexOf(o.state_root, 'state_root', 32),
  };
}

// ---------------------------------------------------------------------------------------------
// Clients

/** JSON-RPC client of a fast-lane node (method names carry the `ksana_` prefix). */
export class KsanaRpcClient {
  /** The underlying JSON-RPC client. */
  readonly rpc: JsonRpcClient;

  constructor(url: string, options: RpcClientOptions = {}) {
    this.rpc = new JsonRpcClient(url, options);
  }

  /**
   * Submits a path B VmOp (`ksana_submitOp`, the only parameter): `op` is the VmOp or its Borsh encoding. Returns the `op_id`
   * computed by the node. Path C operations are submitted through a BTC native transaction (`buildNativeTx`), not through this
   * method.
   */
  async submitOp(op: VmOp | Uint8Array): Promise<{ op_id: Uint8Array }> {
    const encoded = op instanceof Uint8Array ? op : encodeVmOp(op);
    const result = record(await this.rpc.request('ksana_submitOp', [toHex(encoded)]), 'submitOp', ['op_id']);
    return { op_id: hexOf(result.op_id, 'op_id', 32) };
  }

  /** Operation status (`ksana_getOpStatus`); an unknown operation throws `RpcError` (`-32004`). */
  async getOpStatus(opId: Uint8Array | string): Promise<OpStatus> {
    return parseOpStatus(await this.rpc.request('ksana_getOpStatus', [bytesParam(opId, 32, 'op_id')]));
  }

  /**
   * An account's operations on the main chain, newest first (`ksana_getAccountOps`, D-111): those it signed, the asset transfers to
   * it, the calls to it as a contract, and those whose standard events name it or were emitted by it (D-120). Continue with
   * `before: page.next` until `next` is `null`. A node older than D-111, or one whose operation index is not built, throws
   * `RpcError` (`METHOD_NOT_FOUND`).
   */
  async getAccountOps(account: Uint8Array | string, options: OpListOptions = {}): Promise<OpList> {
    return parseOpList(await this.rpc.request('ksana_getAccountOps', [bytesParam(account, 32, 'account'), ...listParams(options)]));
  }

  /** Every operation on the main chain, newest first (`ksana_getLatestOps`, D-111); paged like `getAccountOps`. */
  async getLatestOps(options: OpListOptions = {}): Promise<OpList> {
    return parseOpList(await this.rpc.request('ksana_getLatestOps', listParams(options)));
  }

  /**
   * A contract's code hash, deployer and the standards it declared (`ksana_getContract`, D-120); an account that is no contract
   * throws `RpcError` (`-32004`). The methods of the node's index of the standards' events (this one, `getAccountAssets`,
   * `getToken`, `getTokenHolders`, `getNfts`, `getApprovals`) throw `RpcError` (`METHOD_NOT_FOUND`) on a node older than D-120,
   * or one whose index is not built.
   */
  async getContract(contract: Uint8Array | string): Promise<ContractInfo> {
    return parseContractInfo(await this.rpc.request('ksana_getContract', [bytesParam(contract, 32, 'contract')]));
  }

  /**
   * The assets an account holds, by the node's index (`ksana_getAccountAssets`, D-120): the built-in assets by asset ID, then the
   * KS-20 tokens and KS-721 collections by contract ID. Continue with `after: page.next` until `next` is `null`. The index answers
   * what an account holds; the contracts' own views (`ks20BalanceOf`) answer how much, as a transfer needs it.
   */
  async getAccountAssets(account: Uint8Array | string, options: IndexPageOptions = {}): Promise<AccountAssets> {
    return parseAccountAssets(
      await this.rpc.request('ksana_getAccountAssets', [bytesParam(account, 32, 'account'), ...pageParams(options, ASSET_CURSOR_BYTES)]),
    );
  }

  /**
   * What the node's index knows of a KS-20 token or a KS-721 collection (`ksana_getToken`, D-120); a contract that declared
   * neither throws `RpcError` (`-32004`).
   */
  async getToken(contract: Uint8Array | string): Promise<TokenInfo> {
    return parseTokenInfo(await this.rpc.request('ksana_getToken', [bytesParam(contract, 32, 'contract')]));
  }

  /** A token's or a collection's holders, the largest balance first (`ksana_getTokenHolders`, D-120); paged like `getAccountAssets`. */
  async getTokenHolders(contract: Uint8Array | string, options: IndexPageOptions = {}): Promise<TokenHolders> {
    return parseTokenHolders(
      await this.rpc.request('ksana_getTokenHolders', [bytesParam(contract, 32, 'contract'), ...pageParams(options, HOLDER_CURSOR_BYTES)]),
    );
  }

  /** The NFTs an account holds, of one collection with `contract` (`ksana_getNfts`, D-120); paged like `getAccountAssets`. */
  async getNfts(account: Uint8Array | string, options: NftListOptions = {}): Promise<NftList> {
    const contract = options.contract === undefined || options.contract === null ? null : bytesParam(options.contract, 32, 'contract');
    const [after, limit] = pageParams(options, NFT_CURSOR_BYTES);
    if (contract !== null && after !== null && !after.startsWith(contract)) {
      throw new Error("after must name the contract given, a page's next");
    }
    return parseNftList(await this.rpc.request('ksana_getNfts', [bytesParam(account, 32, 'account'), contract, after, limit]));
  }

  /**
   * The approvals an account gave: KS-20 allowances, KS-721 operators and the approved accounts of its NFTs, as their latest
   * events set them, expired or not (`ksana_getApprovals`, D-120); paged like `getAccountAssets`. What is left of an allowance is
   * the token's `allowance` view (`ks20Allowance`).
   */
  async getApprovals(account: Uint8Array | string, options: IndexPageOptions = {}): Promise<ApprovalList> {
    return parseApprovalList(
      await this.rpc.request('ksana_getApprovals', [bytesParam(account, 32, 'account'), ...pageParams(options, APPROVAL_CURSOR_BYTES)]),
    );
  }

  /** A block (`ksana_getBlock`); defaults to the latest block. */
  async getBlock(id: BlockId = 'latest'): Promise<BlockInfo> {
    return parseBlockInfo(await this.rpc.request('ksana_getBlock', [blockIdParam(id)]));
  }

  /** The account's nonce, balances, and not yet unlocked sponsored fees and block rewards (`ksana_getAccount`). */
  async getAccount(account: Uint8Array | string): Promise<AccountInfo> {
    return parseAccountInfo(await this.rpc.request('ksana_getAccount', [bytesParam(account, 32, 'account')]));
  }

  /**
   * The asset's name, symbol, decimals, total supply, issuing contract, the capabilities it holds and the pause (`ksana_getAsset`,
   * D-074, D-121); a nonexistent asset throws `RpcError` (`-32004`).
   */
  async getAsset(asset: Uint8Array | string): Promise<AssetInfo> {
    return parseAssetInfo(await this.rpc.request('ksana_getAsset', [bytesParam(asset, 32, 'asset')]));
  }

  /**
   * An account's standing in an asset: its status flags and whether the asset's rules let it send and receive the asset now
   * (`ksana_getAssetStatus`, D-121); a nonexistent asset throws `RpcError` (`-32004`).
   */
  async getAssetStatus(asset: Uint8Array | string, account: Uint8Array | string): Promise<AssetStatus> {
    return parseAssetStatus(
      await this.rpc.request('ksana_getAssetStatus', [bytesParam(asset, 32, 'asset'), bytesParam(account, 32, 'account')]),
    );
  }

  /** The main-chain tip, the protocol version and the next scheduled upgrade (`ksana_getChainInfo`, D-076). */
  async getChainInfo(): Promise<ChainInfo> {
    return parseChainInfo(await this.rpc.request('ksana_getChainInfo', []));
  }

  /**
   * The parameters of the node's network, by the chain ID of `ksana_getChainInfo` (`paramsForChainId`, A2-11, D-106): read them
   * before signing and pass them as `params` (and `chain_id`) to what signs and builds operations. With `expected`, a node on
   * another network throws `ChainMismatchError`, as does a chain ID of no known network.
   */
  async getParams(expected?: Params): Promise<Params> {
    return paramsForChainId((await this.getChainInfo()).chain_id, expected);
  }

  /** Read-only contract call (`ksana_call`); returns the receipt. */
  async call(request: CallRequest): Promise<Receipt> {
    const body: Record<string, unknown> = {
      contract: bytesParam(request.contract, 32, 'contract'),
      method: request.method,
      args: toHex(request.args),
    };
    if (request.funds !== undefined) body.funds = request.funds.map((f) => toJson('Funds', f));
    if (request.caller !== undefined) body.caller = bytesParam(request.caller, 32, 'caller');
    if (request.max_gas !== undefined) body.max_gas = request.max_gas.toString();
    return parseReceipt(await this.rpc.request('ksana_call', [body]));
  }

  /**
   * The block's settlement transactions and their BTC status (`ksana_getSettlement`): for a block without a settlement
   * transaction, the settlement transaction that carries its body (D-077); throws `RpcError` (`-32004`) when an empty block or a
   * block containing only force-included entries carries no settlement transaction.
   */
  async getSettlement(id: BlockId = 'latest'): Promise<SettlementInfo> {
    return parseSettlementInfo(await this.rpc.request('ksana_getSettlement', [blockIdParam(id)]));
  }

  /** Suggested path B token amount and the settlement transaction fee rate (`ksana_estimateFee`, D-054); takes no parameters. */
  async estimateFee(): Promise<FeeEstimate> {
    return parseFeeEstimate(await this.rpc.request('ksana_estimateFee', []));
  }
}

/** JSON-RPC client of the standalone indexer (method names carry the `indexer_` prefix, D-048). */
export class IndexerRpcClient {
  /** The underlying JSON-RPC client. */
  readonly rpc: JsonRpcClient;

  constructor(url: string, options: RpcClientOptions = {}) {
    this.rpc = new JsonRpcClient(url, options);
  }

  /** Indexer status (`indexer_status`). */
  async status(): Promise<IndexerStatus> {
    return parseIndexerStatus(await this.rpc.request('indexer_status', []));
  }

  /**
   * Hash and state root of a block that has been executed and verified (`indexer_stateRoot`); any other block throws `RpcError`
   * (`-32004`).
   */
  async stateRoot(id: BlockId = 'latest'): Promise<StateRootInfo> {
    return parseStateRootInfo(await this.rpc.request('indexer_stateRoot', [blockIdParam(id)]));
  }
}

/** Minimal WebSocket interface (the global `WebSocket` of browsers and Node.js ≥ 22). */
export interface WebSocketLike {
  send(data: string): void;
  close(): void;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  onclose: ((ev: unknown) => void) | null;
}

/** Options of `subscribeBlocks`. */
export interface SubscribeOptions {
  /** Replaces the global `WebSocket`. */
  WebSocket?: new (url: string) => WebSocketLike;
  /** Connection or subscription error. */
  onError?: (error: Error) => void;
}

/**
 * Subscribes to new blocks over WebSocket (`ksana_subscribeBlocks`; `url` like `ws://127.0.0.1:8545`). Returns a handle that
 * cancels the subscription.
 */
export function subscribeBlocks(
  url: string,
  onBlock: (block: BlockSummary) => void,
  options: SubscribeOptions = {},
): { close(): void } {
  const Ctor = options.WebSocket ?? (globalThis as { WebSocket?: new (url: string) => WebSocketLike }).WebSocket;
  if (Ctor === undefined) throw new Error('WebSocket is not available in this environment; provide it via options.WebSocket');
  const socket = new Ctor(url);
  let subscription: unknown = undefined;
  const report = (e: Error): void => options.onError?.(e);
  socket.onopen = () => socket.send(JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'ksana_subscribeBlocks', params: [] }));
  socket.onerror = () => report(new RpcTransportError('WebSocket connection error'));
  socket.onmessage = (event) => {
    let message: Obj;
    try {
      message = obj(JSON.parse(String(event.data)), 'message');
    } catch (e) {
      report(e instanceof Error ? e : new Error(String(e)));
      return;
    }
    if (message.id === 1) {
      if (message.error !== undefined && message.error !== null) {
        const error = obj(message.error, 'error');
        report(new RpcError(typeof error.code === 'number' ? error.code : RPC_ERROR_CODES.INTERNAL, String(error.message), error.data));
      } else {
        subscription = message.result;
      }
      return;
    }
    const params = message.params;
    if (typeof params !== 'object' || params === null) return;
    const p = params as Obj;
    if (subscription === undefined || p.subscription !== subscription) return;
    try {
      onBlock(parseBlockSummary(p.result));
    } catch (e) {
      report(e instanceof Error ? e : new Error(String(e)));
    }
  };
  return { close: () => socket.close() };
}
