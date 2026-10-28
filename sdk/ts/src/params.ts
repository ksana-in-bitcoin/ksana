/**
 * Protocol parameters (SPEC §13), one-to-one with `crates/ksana-primitives/src/params.rs`.
 *
 * - `DEVNET`: the devnet defaults (chain ID 1001).
 * - `SIGNET`: the network on BTC signet; differs from `DEVNET` only in the chain ID (1002, D-055).
 * - `MAINNET`: mainnet (chain ID 1000, D-096): 3 confirmations for path C, PoW limit 0x1f00ffff, KSA issuance per D-089 and D-105, placeholders for the
 *   block share, which is not published yet (D-134); everything else as `DEVNET`.
 *
 * Nothing defaults to a network: what signs, builds or decodes takes the parameters of the node it talks to, which
 * `paramsForChainId` / `KsanaRpcClient.getParams` pick by the node's chain ID (A2-11, D-106).
 *
 * Byte lengths and small counts use `number`; the gas parameters that go with the `u64` fields of a VmOp use `bigint`.
 */

/** Argon2id parameters (D-094, consensus parameters), the same as Rust's `Argon2Params`. */
export interface Argon2Params {
  /** `POW_ARGON2_MEMORY`: memory (KiB), that is, the number of 1 KiB blocks; at least `8 × lanes`. */
  readonly memory_kib: number;
  /** `POW_ARGON2_ITERATIONS`: passes (at least 1). */
  readonly iterations: number;
  /** `POW_ARGON2_LANES`: parallelism (1 to 2^24 − 1); only affects how the memory is filled. */
  readonly lanes: number;
}

/** Mining algorithm (D-062): currently only Argon2id (D-094), the same as Rust's `PowAlgorithmId` (externally tagged form). See `pow.ts` for computing the PoW value. */
export type PowAlgorithmId = { readonly argon2id: Argon2Params };

/** The protocol parameters of one network. */
export interface Params {
  /** `CHAIN_ID`: the chain ID, written into every VmOp to prevent cross-network replay. */
  readonly chain_id: number;
  /** `MAGIC`: the OP_RETURN data marker (4 bytes, `KSN1`). */
  readonly magic: Uint8Array;
  /** `TARGET_BLOCK_TIME`: the fast-lane target block interval (seconds). */
  readonly target_block_time_secs: number;
  /** `ASERT_HALF_LIFE`: the difficulty adjustment half-life (seconds). */
  readonly asert_half_life_secs: number;
  /** `SETTLEMENT_DEPTH`: fast-lane confirmations (blocks) required before broadcasting the settlement transaction. */
  readonly settlement_depth: number;
  /** `SETTLEMENT_TIMEOUT`: the threshold of the settlement backlog limit (BTC blocks). */
  readonly settlement_timeout: number;
  /** `NATIVE_CONFIRMATIONS`: BTC confirmations required before a native transaction is force-included. */
  readonly native_confirmations: number;
  /** `MAX_ANCHOR_LAG`: how many blocks a new block's anchor may lag behind the local BTC tip. */
  readonly max_anchor_lag: number;
  /** `BTC_QUERY_DEPTH`: the minimum depth of BTC block headers a contract may read. */
  readonly btc_query_depth: number;
  /**
   * `BTC_FINALITY_CONFS`: confirmations required to be determined final, and then of the BTC block of that determination to be
   * reported BTC-final (D-114).
   */
  readonly btc_finality_confs: number;
  /** `MIN_SETTLEMENT_FEERATE`: the minimum fee rate of settlement transactions (sat/vB). */
  readonly min_settlement_feerate_sat_vb: number;
  /** `BLOCK_GAS_LIMIT`: the gas limit per block. */
  readonly block_gas_limit: bigint;
  /** `NATIVE_MAX_GAS`: the gas limit of a single path C operation. */
  readonly native_max_gas: bigint;
  /** `MAX_OP_BYTES`: the maximum encoded length of a single VmOp (bytes). */
  readonly max_op_bytes: number;
  /** `MAX_BLOCK_DATA_BYTES`: the total data a single block writes into the settlement transaction, also the decoding limit of the three payload types (D-005). */
  readonly max_block_data_bytes: number;
  /** `MAX_SETTLEMENT_HEADERS`: the maximum number of block headers a single settlement transaction carries. */
  readonly max_settlement_headers: number;
  /** `MAX_WASM_MEMORY`: the linear memory limit of a contract (bytes). */
  readonly max_wasm_memory_bytes: number;
  /** `MAX_CALL_DEPTH`: the depth limit of contract-to-contract calls. */
  readonly max_call_depth: number;
  /** `REORG_WINDOW`: the number of fast-lane blocks that can be rolled back. */
  readonly reorg_window: number;
  /** The number of previous blocks whose median timestamp a new block's timestamp must exceed (D-006). */
  readonly median_time_span: number;
  /** How many seconds at most a new block's timestamp may be ahead of local time (D-006). */
  readonly max_future_block_time_secs: number;
  /** The compact representation of the PoW target limit (D-025). */
  readonly pow_limit_bits: number;
  /** `POW_ALGORITHM` (D-062, D-094): the mining algorithm and its parameters; the salt uses `chain_id`. */
  readonly pow_algorithm: PowAlgorithmId;
  /** `BLOCK_REWARD` (D-074): the block reward per block at DAA score 0 (KSA base units). */
  readonly block_reward: bigint;
  /** `REWARD_HALVING_INTERVAL` (D-074, D-105): the length in DAA score of the first halving period; with `REWARD_INTERVAL_DOUBLINGS` 0 every halving period has this length. */
  readonly reward_halving_interval: bigint;
  /**
   * `REWARD_INTERVAL_DOUBLINGS` (D-105): how many times the halving period doubles in length. Halving period k (k = 0, 1, …) lasts
   * `REWARD_HALVING_INTERVAL × 2^min(k, REWARD_INTERVAL_DOUBLINGS)` DAA scores and pays `BLOCK_REWARD >> k` per reward slot: with 0
   * every period is `REWARD_HALVING_INTERVAL` long (D-074's schedule), mainnet's 2 gives periods 1, 2, 4, 4, 4, … intervals long.
   */
  readonly reward_interval_doublings: number;
  /** `KSA_MAX_SUPPLY` (D-074): the limit on the genesis allocation plus all block rewards (KSA base units). */
  readonly ksa_max_supply: bigint;
  /**
   * `BLOCK_SHARE_BPS` (D-089, base value, varies with the protocol version): the block share's basis points of each reward
   * slot. For reward slots with a DAA score below `BLOCK_SHARE_END`, `reward × BLOCK_SHARE_BPS / 10 000` (rounded down) is
   * credited to the block-share account in the genesis config and the rest goes to the producer; without that account
   * everything goes to the producer.
   */
  readonly block_share_bps: number;
  /** `BLOCK_SHARE_END` (D-089, base value, varies with the protocol version): the DAA score up to which (exclusive) the block share applies. */
  readonly block_share_end: bigint;
  /**
   * `CARRIER_BPS` (D-095, base value, varies with the protocol version): the carrier reward's basis points of each reward slot.
   * The sender of the payload that first landed the block's data on chain (the carrier) receives
   * `reward × CARRIER_BPS / 10 000` (rounded down) of each of the block's reward slots at unlock time, taken from the
   * producer's portion; the block share's portion is unchanged.
   */
  readonly carrier_bps: number;
  /** `CARRIER_FEE_BPS` (D-095, base value, varies with the protocol version): the carrier's basis points of the block's path B sponsored fees; the rest goes to the producer. */
  readonly carrier_fee_bps: number;
  /**
   * `CARRIER_REPOST_DELAY` (D-095, fixed per network): for the payee of a republished payload to receive the carrier reward,
   * the block's data must have landed on chain only after waiting more than `ANCHOR_DEPTH + CARRIER_REPOST_DELAY` BTC blocks
   * past the anchor.
   */
  readonly carrier_repost_delay: number;
  /** The upgrade schedule (D-076): strictly increasing in `daa_score` and `version`; with an empty schedule the base version `BLOCK_VERSION` applies throughout. */
  readonly upgrades: readonly Upgrade[];
}

/**
 * One network upgrade (D-076, the same as Rust's `Upgrade`): from the block whose DAA score reaches `daa_score`, the protocol
 * version (block header version) is `version`; the rules given override the previous values (omitted or `null` means
 * unchanged).
 */
export interface Upgrade {
  readonly version: number;
  readonly daa_score: bigint;
  readonly block_gas_limit?: bigint | null;
  readonly native_max_gas?: bigint | null;
  readonly min_vmop_version?: number | null;
  readonly max_vmop_version?: number | null;
  readonly block_share_bps?: number | null;
  readonly block_share_end?: bigint | null;
  readonly carrier_bps?: number | null;
  readonly carrier_fee_bps?: number | null;
}

/** The version-dependent rules that apply to a block (D-076, the same as Rust's `Rules`), obtained from `rulesAt`. */
export interface Rules {
  /** The protocol version: the header's `version` must equal it. */
  readonly version: number;
  readonly block_gas_limit: bigint;
  readonly native_max_gas: bigint;
  /** The accepted range of VmOp versions (inclusive). */
  readonly min_vmop_version: number;
  readonly max_vmop_version: number;
  /** The block share ratio (basis points) and its cutoff DAA score (D-089). */
  readonly block_share_bps: number;
  readonly block_share_end: bigint;
  /** The carrier reward's basis points of each reward slot and of the path B sponsored fees (D-095). */
  readonly carrier_bps: number;
  readonly carrier_fee_bps: number;
}

/** The three portions a block's block reward splits into (D-089, D-095, the same as Rust's `RewardSplit`). */
export interface RewardSplit {
  /** The producer's portion. */
  readonly producer: bigint;
  /** The block-share account's portion (D-089). */
  readonly share: bigint;
  /** The carrier's portion (D-095): goes to the producer when the block has no carrier. */
  readonly carrier: bigint;
}

/** `amount × bps / 10 000` (rounded down; a ratio above 100% counts as 100%). */
function bpsOf(amount: bigint, bps: number): bigint {
  const ratio = BigInt(Math.min(Math.max(Math.trunc(bps), 0), 10_000));
  return (amount * ratio) / 10_000n;
}

const KIB = 1024;
const MIB = 1024 * 1024;
const U128_MAX = (1n << 128n) - 1n;

/** The devnet default parameters (SPEC §13). */
export const DEVNET: Params = Object.freeze({
  chain_id: 1001,
  magic: Uint8Array.of(0x4b, 0x53, 0x4e, 0x31), // "KSN1"
  target_block_time_secs: 5,
  asert_half_life_secs: 7200, // D-090: scaled up with the empty-block weight limit
  settlement_depth: 2,
  settlement_timeout: 6,
  native_confirmations: 1,
  max_anchor_lag: 1,
  btc_query_depth: 6,
  btc_finality_confs: 6,
  min_settlement_feerate_sat_vb: 1,
  block_gas_limit: 100_000_000n,
  native_max_gas: 1_000_000n,
  max_op_bytes: 64 * KIB,
  max_block_data_bytes: 80 * KIB,
  max_settlement_headers: 500, // D-075
  max_wasm_memory_bytes: 16 * MIB,
  max_call_depth: 8,
  reorg_window: 1000,
  median_time_span: 11,
  max_future_block_time_secs: 15,
  pow_limit_bits: 0x207f_ffff,
  // Argon2id (D-094): 2 MiB, 1 pass, 1 lane: about 1 ms per header single-threaded on an ordinary CPU, so a browser can verify a single header
  pow_algorithm: Object.freeze({ argon2id: Object.freeze({ memory_kib: 2048, iterations: 1, lanes: 1 }) }),
  // Placeholder values of D-074: 9.9 KSA (8 decimals), halving every 1 460 days (one block per 5 seconds; the halving periods do not
  // double, D-105), max supply 1 billion KSA
  block_reward: 990_000_000n,
  reward_halving_interval: 25_228_800n,
  reward_interval_doublings: 0,
  ksa_max_supply: 100_000_000_000_000_000n,
  // Block share (D-089): for the first two halving periods, 20% of each reward slot is credited to the block-share account in the genesis config
  block_share_bps: 2_000,
  block_share_end: 50_457_600n,
  // Carrier reward (D-095): 5% of each reward slot is taken from the producer's portion for the producer who landed the block's
  // data on chain, the path B sponsored fees are split in half; the payee of a republish has to wait until the block's data has
  // waited more than ANCHOR_DEPTH + 2 BTC blocks past the anchor
  carrier_bps: 500,
  carrier_fee_bps: 5_000,
  carrier_repost_delay: 2,
  // No scheduled upgrades (D-076)
  upgrades: Object.freeze([]),
});

/** The signet parameters: differ from devnet only in the chain ID (D-055). */
export const SIGNET: Params = Object.freeze({ ...DEVNET, chain_id: 1002 });

/**
 * Mainnet parameters (the mainnet column of SPEC §13, D-096), identical to Rust's `MAINNET` in the fields the SDK carries. Differences from devnet: chain ID 1000; `NATIVE_CONFIRMATIONS` 3
 * (a path C op is included about 5 BTC blocks after its carrying transaction); PoW limit 0x1f00ffff (a target of about 2^240); KSA issuance per D-089 and D-105: a max
 * supply of 210 million KSA, 4.2 KSA per slot, halving periods of 12 500 000, 25 000 000 and then 50 000 000 DAA score each (about 2, 4 and then 8 years). The block
 * share (D-089) is not published yet (D-134): `block_share_bps` and `block_share_end` are placeholders (0, no share), as in Rust, where a mainnet node refuses to
 * start until they are filled in.
 */
export const MAINNET: Params = Object.freeze({
  ...DEVNET,
  chain_id: 1000,
  native_confirmations: 3,
  pow_limit_bits: 0x1f00_ffff,
  block_reward: 420_000_000n,
  reward_halving_interval: 12_500_000n,
  reward_interval_doublings: 2,
  ksa_max_supply: 21_000_000_000_000_000n,
  block_share_bps: 0,
  block_share_end: 0n,
});

/** A network by name: the value of `--chain` of the node, the indexer and the CLI (D-055, D-096). */
export type NetworkName = 'devnet' | 'signet' | 'mainnet';

/** The parameters of each network, by name. */
export const NETWORKS: Readonly<Record<NetworkName, Params>> = Object.freeze({ devnet: DEVNET, signet: SIGNET, mainnet: MAINNET });

/** The network whose chain ID is `chainId`; `null` when it belongs to no network (the same as Rust's `Network::from_chain_id`). */
export function networkOfChainId(chainId: number): NetworkName | null {
  for (const name of Object.keys(NETWORKS) as NetworkName[]) {
    if (NETWORKS[name].chain_id === chainId) return name;
  }
  return null;
}

/**
 * A chain ID that is not the one expected (A2-11, D-106): a node on another network, or on none, or a VmOp signed for another
 * network. Signing for one network and sending to another loses a path C payment: the KSN1 magic is the same on every network,
 * so the node force-includes the native transaction, the operation fails on its chain ID, and the BTC payment confirms anyway.
 */
export class ChainMismatchError extends Error {
  constructor(
    /** The chain ID found: the node's, or the operation's. */
    readonly chainId: number,
    /** The chain ID expected; `null` when any of the known networks would do. */
    readonly expected: number | null,
    message: string,
  ) {
    super(message);
    this.name = 'ChainMismatchError';
  }
}

/** Display name of a chain ID: the network's name, or the bare chain ID. */
function chainName(chainId: number): string {
  const name = networkOfChainId(chainId);
  return name === null ? `chain ID ${chainId}` : `${name} (chain ID ${chainId})`;
}

/**
 * The parameters of the network a node is on, from the chain ID it reports (`chain_id` of `ksana_getChainInfo`, A2-11, D-106;
 * `KsanaRpcClient.getParams` reads it): `expected` when given and on that chain ID, otherwise the network's own parameters.
 * Throws `ChainMismatchError` when the chain ID belongs to no network, or to another one than `expected`.
 */
export function paramsForChainId(chainId: number, expected?: Params): Params {
  const name = networkOfChainId(chainId);
  if (name === null) {
    const known = (Object.keys(NETWORKS) as NetworkName[]).map((n) => `${n} ${NETWORKS[n].chain_id}`).join(', ');
    throw new ChainMismatchError(chainId, expected?.chain_id ?? null, `chain ID ${chainId} belongs to no Ksana network (${known})`);
  }
  if (expected !== undefined && expected.chain_id !== chainId) {
    throw new ChainMismatchError(
      chainId,
      expected.chain_id,
      `the node is on ${chainName(chainId)}, not ${chainName(expected.chain_id)}: refusing to sign for one network and send to another`,
    );
  }
  return expected ?? NETWORKS[name];
}

/**
 * The halving period of the reward slot with DAA score `daaScore` (D-074, D-105, the same as `Params::reward_period_at`): period k
 * (k = 0, 1, …) is `REWARD_HALVING_INTERVAL × 2^min(k, REWARD_INTERVAL_DOUBLINGS)` DAA scores long, so the first
 * `REWARD_INTERVAL_DOUBLINGS` periods double in length and every later one is as long as the last of them; with 0 doublings it is
 * `⌊daaScore / REWARD_HALVING_INTERVAL⌋`. `null` when the interval is 0 (no reward).
 */
export function rewardPeriodAt(params: Params, daaScore: bigint): bigint | null {
  if (daaScore < 0n) throw new RangeError('DAA score must not be negative');
  let len = params.reward_halving_interval;
  if (len === 0n) return null;
  let rest = daaScore;
  let period = 0n;
  while (period < BigInt(params.reward_interval_doublings)) {
    if (rest < len) return period;
    rest -= len;
    len *= 2n;
    period += 1n;
  }
  return period + rest / len;
}

/**
 * The block reward of a block with DAA score `daaScore` (D-074, D-105, the same as `Params::block_reward_at`): `BLOCK_REWARD >> k`
 * for its halving period k (`rewardPeriodAt`), 0 from the 128th halving on; no reward when the interval is 0.
 */
export function blockRewardAt(params: Params, daaScore: bigint): bigint {
  const period = rewardPeriodAt(params, daaScore);
  return period === null || period >= 128n ? 0n : params.block_reward >> period;
}

/**
 * The upper bound on the sum of all block rewards (D-074, D-105, the same as `Params::max_total_rewards`):
 * `(REWARD_INTERVAL_DOUBLINGS + 2) × BLOCK_REWARD × REWARD_HALVING_INTERVAL`, at most 2^128 − 1.
 */
export function maxTotalRewards(params: Params): bigint {
  const bound = (BigInt(params.reward_interval_doublings) + 2n) * params.block_reward * params.reward_halving_interval;
  return bound > U128_MAX ? U128_MAX : bound;
}

/**
 * The block-share account's portion of the reward slot with DAA score `daaScore` (D-089, the same as
 * `Params::block_share_at`): by the slot's own rules (`rulesAt`), `reward × BLOCK_SHARE_BPS / 10 000` (rounded down, a ratio
 * above 100% counts as 100%) when the DAA score is below `BLOCK_SHARE_END`, otherwise 0.
 */
export function blockShareAt(params: Params, daaScore: bigint): bigint {
  const rules = rulesAt(params, daaScore);
  if (daaScore >= rules.block_share_end) return 0n;
  return bpsOf(blockRewardAt(params, daaScore), rules.block_share_bps);
}

/**
 * The carrier's portion of the reward slot with DAA score `daaScore` (D-095, the same as `Params::block_carrier_at`): by the
 * slot's own rules, `reward × CARRIER_BPS / 10 000` (rounded down), at most the remainder after deducting the block share.
 */
export function blockCarrierAt(params: Params, daaScore: bigint): bigint {
  const reward = blockRewardAt(params, daaScore);
  const rest = reward - blockShareAt(params, daaScore);
  const carrier = bpsOf(reward, rulesAt(params, daaScore).carrier_bps);
  return carrier < rest ? carrier : rest;
}

/**
 * The carrier's portion of a block's path B sponsored fees `amount` (D-095, the same as `Params::carrier_fee_at`): by the block's
 * own rules (DAA score `daaScore`), `amount × CARRIER_FEE_BPS / 10 000` (rounded down); the rest goes to the producer.
 */
export function carrierFeeAt(params: Params, daaScore: bigint, amount: bigint): bigint {
  if (amount < 0n) throw new RangeError('amount must not be negative');
  return bpsOf(amount, rulesAt(params, daaScore).carrier_fee_bps);
}

/**
 * The block reward of a block with DAA score `daaScore` and weight `weight`, split into the producer's, the block-share
 * account's and the carrier's portions (D-089, D-095, the same as `Params::block_rewards_split_at`): over the slots
 * `daaScore`, `daaScore − 1`, … (not below 0), the sum of `blockShareAt` goes to the share account, the sum of
 * `blockCarrierAt` to the carrier, and the rest to the producer.
 */
export function blockRewardsSplitAt(params: Params, daaScore: bigint, weight: bigint): RewardSplit {
  if (daaScore < 0n || weight < 0n) throw new RangeError('DAA score and weight must not be negative');
  let producer = 0n;
  let share = 0n;
  let carrier = 0n;
  for (let i = 0n; i < weight && i <= daaScore; i++) {
    const slot = daaScore - i;
    const slotShare = blockShareAt(params, slot);
    const slotCarrier = blockCarrierAt(params, slot);
    producer += blockRewardAt(params, slot) - slotShare - slotCarrier;
    share += slotShare;
    carrier += slotCarrier;
  }
  return { producer, share, carrier };
}

/** The current VmOp version (SPEC §4.1): 2 is `Deploy` / `Call` with attached funds (D-073). */
export const VMOP_VERSION = 2;

/** The maximum number of attached fund items of one action (D-073). */
export const MAX_CALL_FUNDS = 16;

/** The base block header version (D-027): 2 is the DAG header (D-063); for the version after upgrades see `rulesAt` (D-076). */
export const BLOCK_VERSION = 2;

/**
 * The rules that apply to a block with DAA score `daaScore` (D-076, the same as `Params::rules`): starting from the base rules
 * (the values in the parameters, protocol version `BLOCK_VERSION`, VmOp version `VMOP_VERSION`), the upgrades whose
 * `daa_score` does not exceed it are applied in order.
 */
export function rulesAt(params: Params, daaScore: bigint): Rules {
  if (daaScore < 0n) throw new RangeError('DAA score must not be negative');
  let rules: Rules = {
    version: BLOCK_VERSION,
    block_gas_limit: params.block_gas_limit,
    native_max_gas: params.native_max_gas,
    min_vmop_version: VMOP_VERSION,
    max_vmop_version: VMOP_VERSION,
    block_share_bps: params.block_share_bps,
    block_share_end: params.block_share_end,
    carrier_bps: params.carrier_bps,
    carrier_fee_bps: params.carrier_fee_bps,
  };
  for (const u of params.upgrades) {
    if (u.daa_score > daaScore) continue;
    rules = {
      version: u.version,
      block_gas_limit: u.block_gas_limit ?? rules.block_gas_limit,
      native_max_gas: u.native_max_gas ?? rules.native_max_gas,
      min_vmop_version: u.min_vmop_version ?? rules.min_vmop_version,
      max_vmop_version: u.max_vmop_version ?? rules.max_vmop_version,
      block_share_bps: u.block_share_bps ?? rules.block_share_bps,
      block_share_end: u.block_share_end ?? rules.block_share_end,
      carrier_bps: u.carrier_bps ?? rules.carrier_bps,
      carrier_fee_bps: u.carrier_fee_bps ?? rules.carrier_fee_bps,
    };
  }
  return rules;
}

/** The next scheduled upgrade after the block with DAA score `daaScore` (`null` when there is none). */
export function nextUpgrade(params: Params, daaScore: bigint): Upgrade | null {
  return params.upgrades.find((u) => u.daa_score > daaScore) ?? null;
}

/** The asset ID of the project token KSA: 32 zero bytes (SPEC §4.4). Returns a new copy. */
export function ksaAssetId(): Uint8Array {
  return new Uint8Array(32);
}
