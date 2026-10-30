//! Protocol parameters (SPEC §13). All parameters are defined here in one place, per network.
//!
//! Three networks: devnet (BTC regtest), signet (the M6 browser wallet acceptance, D-055; differs from devnet only in
//! `chain_id`, `ANCHOR_DEPTH`, D-056, and the PoW limit, D-136) and mainnet (the mainnet column of SPEC §13 and the
//! parameter rationale of §13.1, D-096; see [`MAINNET`] for the differences from devnet).
//! Consensus parameters beyond the spec (PoW limit, mining algorithm, GHOSTDAG, block rewards, two-speed difficulty
//! D-084 / D-090, upgrade table) have their source noted in the field docs of [`Params`].

/// The fast-lane network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// The devnet running on BTC regtest.
    Devnet,
    /// The testnet running on BTC signet (D-055).
    Signet,
    /// The mainnet, running on BTC mainnet (D-096).
    Mainnet,
}

impl Network {
    /// All networks.
    pub const ALL: [Network; 3] = [Network::Devnet, Network::Signet, Network::Mainnet];

    /// The protocol parameters of this network.
    pub const fn params(self) -> &'static Params {
        match self {
            Network::Devnet => &DEVNET,
            Network::Signet => &SIGNET,
            Network::Mainnet => &MAINNET,
        }
    }

    /// Name of the network (the value of `--chain`): `devnet`, `signet` or `mainnet`.
    pub const fn name(self) -> &'static str {
        match self {
            Network::Devnet => "devnet",
            Network::Signet => "signet",
            Network::Mainnet => "mainnet",
        }
    }

    /// Look up a network by name: `devnet`, `signet` or `mainnet`.
    pub fn from_name(name: &str) -> Option<Network> {
        Network::ALL.into_iter().find(|n| n.name() == name)
    }

    /// Look up a network by chain ID (used when the genesis config is checked against the network, D-096); `None`
    /// when the ID belongs to no network.
    pub fn from_chain_id(chain_id: u32) -> Option<Network> {
        Network::ALL
            .into_iter()
            .find(|n| n.params().chain_id == chain_id)
    }
}

/// The mining algorithm (D-062): how the PoW value is computed. The block identity (the block hash
/// `sha256d(borsh(header))`) and the definitions of the target and the work do not change with the algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowAlgorithmId {
    /// Argon2id (RFC 9106, version 0x13, D-094): PoW value = the 32-byte output of
    /// `Argon2id(message = borsh(header), salt = "ksana-pow" ‖ chain_id as 4 little-endian bytes)` interpreted as
    /// little-endian (the same byte order convention as D-025). The parameters are fixed per network and are
    /// consensus parameters.
    Argon2id(Argon2Params),
}

/// The Argon2id parameters (D-094, consensus parameters): they determine the memory and time of one PoW computation
/// per block header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Argon2Params {
    /// `POW_ARGON2_MEMORY`: memory (KiB), i.e. the number of 1 KiB blocks; at least `8 × lanes`.
    pub memory_kib: u32,
    /// `POW_ARGON2_ITERATIONS`: the number of passes (at least 1).
    pub iterations: u32,
    /// `POW_ARGON2_LANES`: the degree of parallelism (1 to 2^24 − 1); it only affects how the memory is filled and does
    /// not require a parallel implementation.
    pub lanes: u32,
}

impl Argon2Params {
    /// The output length of the PoW value (bytes): fixed at 32.
    pub const OUTPUT_LEN: usize = 32;
    /// The fixed prefix of the salt (followed by the chain ID as 4 little-endian bytes).
    pub const SALT_PREFIX: &'static [u8] = b"ksana-pow";

    /// The salt for the network `chain_id`: `"ksana-pow" ‖ chain_id` (4 little-endian bytes), 13 bytes in all.
    pub fn salt(chain_id: u32) -> [u8; 13] {
        let mut salt = [0u8; 13];
        for (dst, src) in salt.iter_mut().zip(
            Self::SALT_PREFIX
                .iter()
                .copied()
                .chain(chain_id.to_le_bytes()),
        ) {
            *dst = src;
        }
        salt
    }
}

/// A network upgrade (D-076): from the blocks whose DAA score reaches `daa_score`, the protocol version (block header
/// version) is `version`; the rules given override the previous values (`None` means unchanged). Changes beyond the
/// rules branch on [`Rules::version`] in the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Upgrade {
    /// The new protocol version.
    pub version: u32,
    /// The activation point: blocks whose DAA score (D-063) is not below it use the new version.
    pub daa_score: u64,
    /// The new `BLOCK_GAS_LIMIT`.
    pub block_gas_limit: Option<u64>,
    /// The new `NATIVE_MAX_GAS`.
    pub native_max_gas: Option<u64>,
    /// The lowest accepted VmOp version.
    pub min_vmop_version: Option<u8>,
    /// The highest accepted VmOp version.
    pub max_vmop_version: Option<u8>,
    /// The new `BLOCK_SHARE_BPS` (D-089).
    pub block_share_bps: Option<u32>,
    /// The new `BLOCK_SHARE_END` (D-089).
    pub block_share_end: Option<u64>,
    /// The new `CARRIER_BPS` (D-095).
    pub carrier_bps: Option<u32>,
    /// The new `CARRIER_FEE_BPS` (D-095).
    pub carrier_fee_bps: Option<u32>,
}

impl Upgrade {
    /// An upgrade that only changes the protocol version, without changing the values in [`Rules`].
    pub const fn new(version: u32, daa_score: u64) -> Upgrade {
        Upgrade {
            version,
            daa_score,
            block_gas_limit: None,
            native_max_gas: None,
            min_vmop_version: None,
            max_vmop_version: None,
            block_share_bps: None,
            block_share_end: None,
            carrier_bps: None,
            carrier_fee_bps: None,
        }
    }
}

/// The rules that apply to a block and vary with the protocol version (D-076), obtained from [`Params::rules`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    /// The protocol version: the header's `version` must equal it.
    pub version: u32,
    /// `BLOCK_GAS_LIMIT`: the gas limit per block.
    pub block_gas_limit: u64,
    /// `NATIVE_MAX_GAS`: the gas limit of a path C native op.
    pub native_max_gas: u64,
    /// The accepted VmOp version range (both ends inclusive).
    pub min_vmop_version: u8,
    /// See `min_vmop_version`.
    pub max_vmop_version: u8,
    /// `BLOCK_SHARE_BPS` (D-089): the block share's fraction of each reward slot in basis points (0 to 10 000).
    pub block_share_bps: u32,
    /// `BLOCK_SHARE_END` (D-089): only reward slots whose DAA score is below it have a block share.
    pub block_share_end: u64,
    /// `CARRIER_BPS` (D-095): the carrier reward's fraction of each reward slot in basis points (taken out of the
    /// producer's share; its sum with `block_share_bps` does not exceed 10 000).
    pub carrier_bps: u32,
    /// `CARRIER_FEE_BPS` (D-095): the carrier's fraction of the path B sponsored fees in basis points (0 to 10 000).
    pub carrier_fee_bps: u32,
}

/// The three parts a block's reward is split into (D-089, D-095): the producer, the block share account, the carrier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RewardSplit {
    /// The part going to the producer.
    pub producer: u128,
    /// The part going to the block share account (D-089).
    pub share: u128,
    /// The part going to the carrier (D-095): it goes to the producer when the block has no carrier.
    pub carrier: u128,
}

impl RewardSplit {
    /// The sum of the three parts (saturating), i.e. [`Params::block_rewards_at`].
    pub fn total(&self) -> u128 {
        self.producer
            .saturating_add(self.share)
            .saturating_add(self.carrier)
    }
}

/// `amount × bps / 10 000` (rounded down; a fraction above 100% counts as 100%): the whole part and the remainder are
/// multiplied separately to avoid overflow.
fn bps_of(amount: u128, bps: u32) -> u128 {
    let bps = u128::from(bps.min(10_000));
    let whole = (amount / 10_000).saturating_mul(bps);
    let rest = (amount % 10_000).saturating_mul(bps) / 10_000;
    whole.saturating_add(rest).min(amount)
}

impl Rules {
    /// Whether VmOps of this version are accepted.
    pub fn accepts_vmop_version(&self, version: u8) -> bool {
        (self.min_vmop_version..=self.max_vmop_version).contains(&version)
    }
}

/// All protocol parameters of one network.
///
/// Fast-lane heights and block counts use `u64` (consistent with `BlockHeader::height`),
/// BTC heights and block counts use `u32` (consistent with `BtcAnchor::height`), byte lengths use `usize`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// `CHAIN_ID`: the chain ID, written into every VmOp, preventing cross-network replay.
    pub chain_id: u32,
    /// `MAGIC`: the OP_RETURN data marker.
    pub magic: [u8; 4],
    /// `TARGET_BLOCK_TIME`: the fast-lane target block interval (seconds).
    pub target_block_time_secs: u64,
    /// `ASERT_HALF_LIFE`: the difficulty adjustment half-life (seconds).
    pub asert_half_life_secs: u64,
    /// `SETTLEMENT_DEPTH`: the fast-lane confirmations (blocks) required before broadcasting the settlement
    /// transaction.
    pub settlement_depth: u64,
    /// `SETTLEMENT_TIMEOUT`: the threshold of the settlement backlog limit (BTC blocks, §6.4).
    pub settlement_timeout: u32,
    /// `NATIVE_CONFIRMATIONS`: the BTC confirmations required before a native transaction is forcibly included.
    pub native_confirmations: u32,
    /// `MAX_ANCHOR_LAG`: how many blocks a new block's anchor may lag behind the local BTC chain tip (beyond
    /// `ANCHOR_DEPTH`, D-056).
    pub max_anchor_lag: u32,
    /// `ANCHOR_DEPTH`: how many blocks below the local BTC chain tip the producer anchors (not given by the spec,
    /// D-056): the deeper the anchored BTC block, the less likely it is to be reorganized away by BTC. A path A funding
    /// input needs `ANCHOR_DEPTH + 1` confirmations to be included.
    pub anchor_depth: u32,
    /// `BTC_QUERY_DEPTH`: the minimum depth of BTC block headers a contract may read.
    pub btc_query_depth: u32,
    /// `BTC_FINALITY_CONFS`: the confirmations required to be judged BTC-final.
    pub btc_finality_confs: u32,
    /// `MIN_SETTLEMENT_FEERATE`: the minimum fee rate of a settlement transaction (sat/vB, consensus rule).
    pub min_settlement_feerate_sat_vb: u64,
    /// `BLOCK_GAS_LIMIT`: the gas limit per block.
    pub block_gas_limit: u64,
    /// `NATIVE_MAX_GAS`: the gas limit of a single path C op.
    pub native_max_gas: u64,
    /// `MAX_OP_BYTES`: the maximum encoded length of a single VmOp (bytes, including contract code).
    pub max_op_bytes: usize,
    /// `MAX_BLOCK_DATA_BYTES`: the total data a single block writes into the settlement transaction (bytes).
    pub max_block_data_bytes: usize,
    /// `MAX_SETTLEMENT_HEADERS`: the maximum number of headers carried by a single settlement transaction.
    pub max_settlement_headers: usize,
    /// `MAX_WASM_MEMORY`: the contract linear memory limit (bytes).
    pub max_wasm_memory_bytes: u64,
    /// `MAX_CALL_DEPTH`: the maximum depth of contract-to-contract calls.
    pub max_call_depth: u32,
    /// `REORG_WINDOW`: the number of fast-lane blocks that can be rolled back.
    pub reorg_window: u64,
    /// §6.3: the number of preceding blocks whose median the new block's timestamp must exceed.
    pub median_time_span: usize,
    /// §6.3: how many seconds at most a new block's timestamp may be ahead of local time.
    pub max_future_block_time_secs: u64,
    /// D-083: how many seconds at most a block's timestamp may be ahead of the time of the BTC block header that
    /// `btc_anchor` points at. Unlike the two rules of §6.3, this is a consensus rule: the anchor hash commits to that
    /// BTC block header, so both a node syncing historical blocks and the indexer can check it.
    pub max_timestamp_ahead_of_anchor_secs: u64,
    /// The compact representation of the PoW target limit (minimum difficulty), also the genesis block's `bits` (not
    /// given by the spec, D-025).
    pub pow_limit_bits: u32,
    /// The mining algorithm (D-062, consensus parameter).
    pub pow_algorithm: PowAlgorithmId,
    /// `GHOSTDAG_K` (D-063): the maximum number of blue blocks in a blue block's anticone.
    pub ghostdag_k: u16,
    /// `MAX_BLOCK_PARENTS` (D-063): the maximum number of parents of a non-genesis block.
    pub max_block_parents: usize,
    /// `MAX_MERGESET_SIZE` (D-063): the maximum size of the merge set (including the selected parent).
    pub max_mergeset_size: usize,
    /// `MERGE_DEPTH` (D-063, blue score): the blue score of a block in the merge set other than the selected parent
    /// must not be below the selected parent's blue score minus it.
    pub merge_depth: u64,
    /// `BLOCK_REWARD` (D-074): the block reward per block at DAA score 0 (KSA base units).
    pub block_reward: u128,
    /// `REWARD_HALVING_INTERVAL` (D-074, D-105): the length in DAA score of the first halving period; with
    /// `REWARD_INTERVAL_DOUBLINGS` 0 every halving period has this length.
    pub reward_halving_interval: u64,
    /// `REWARD_INTERVAL_DOUBLINGS` (D-105): how many times the halving period doubles in length. Halving period k
    /// (k = 0, 1, …) lasts `REWARD_HALVING_INTERVAL × 2^min(k, REWARD_INTERVAL_DOUBLINGS)` DAA scores and pays
    /// `BLOCK_REWARD >> k` per reward slot: with 0 every period is `REWARD_HALVING_INTERVAL` long (D-074's schedule),
    /// mainnet's 2 gives periods 1, 2, 4, 4, 4, … intervals long.
    pub reward_interval_doublings: u32,
    /// `KSA_MAX_SUPPLY` (D-074): the limit on the sum of the genesis allocation and all block rewards (KSA base units).
    pub ksa_max_supply: u128,
    /// `MAX_UNLOCK_BATCH` (D-074): the maximum number of lock records one unlock entry may unlock.
    pub max_unlock_batch: u64,
    /// `BLOCK_SHARE_BPS` (D-089, base value, varies with the protocol version): the block share's fraction of each
    /// reward slot in basis points. For reward slots whose DAA score is below `BLOCK_SHARE_END`,
    /// `reward × BLOCK_SHARE_BPS / 10 000` (rounded down) is credited to the block share account of the genesis config
    /// and the rest goes to the producer; without that account configured everything goes to the producer.
    pub block_share_bps: u32,
    /// `BLOCK_SHARE_END` (D-089, base value, varies with the protocol version): the DAA score up to which (exclusive)
    /// the block share lasts.
    pub block_share_end: u64,
    /// `CARRIER_BPS` (D-095, base value, varies with the protocol version): the carrier reward's fraction of each
    /// reward slot in basis points. The sender of the payload that first lands a block's data on BTC (the carrier)
    /// receives `reward × CARRIER_BPS / 10 000` (rounded down) of each of that block's reward slots at unlock, taken
    /// out of the producer's share; the block share's part is unchanged.
    pub carrier_bps: u32,
    /// `CARRIER_FEE_BPS` (D-095, base value, varies with the protocol version): the carrier's fraction of the block's
    /// path B sponsored fees in basis points; the rest goes to the producer.
    pub carrier_fee_bps: u32,
    /// `CARRIER_REPOST_DELAY` (D-095, fixed per network): for the payee of a republish payload to receive the carrier
    /// reward, the block's data must have landed on BTC only after waiting more than
    /// `ANCHOR_DEPTH + CARRIER_REPOST_DELAY` BTC blocks after its anchor
    /// (`data(B) − B.btc_anchor.height > ANCHOR_DEPTH + CARRIER_REPOST_DELAY`).
    pub carrier_repost_delay: u32,
    /// `EMPTY_BLOCK_DIFFICULTY` (D-084, two-speed difficulty): the difficulty multiplier N of the first empty block
    /// (`entries_root` is the root of the empty entry list) after a non-empty block. The PoW target of an empty block
    /// is the `bits` target divided by its weight w (its work is about w times that of a non-empty block with the same
    /// `bits`); the weight is also the DAA score increment and the number of reward slots; a non-empty block has 1.
    /// The s-th consecutive empty block on the selected chain (empty-block streak, D-090) has weight
    /// `min(N × 2^(s − 1), MAX_EMPTY_BLOCK_WEIGHT)`: empty and non-empty blocks with the same parents have the same
    /// `bits`, so when idle the interval between headers landing on BTC doubles block by block from about N block
    /// intervals up to the limit. Must be at least 1 (0 is treated as 1).
    pub empty_block_difficulty: u64,
    /// `MAX_EMPTY_BLOCK_WEIGHT` (D-090): the limit N_MAX on the empty block weight (difficulty multiplier, reward slot
    /// count), up to which the weight of consecutive empty blocks doubles; when below `EMPTY_BLOCK_DIFFICULTY` the
    /// latter is used.
    pub max_empty_block_weight: u64,
    /// The upgrade table (D-076): strictly increasing in `daa_score` and `version`; with an empty table it is always
    /// the base version [`crate::BLOCK_VERSION`].
    pub upgrades: &'static [Upgrade],
}

impl Params {
    /// The halving period of the reward slot at DAA score `daa_score` (D-074, D-105): period k (k = 0, 1, …) is
    /// `REWARD_HALVING_INTERVAL × 2^min(k, REWARD_INTERVAL_DOUBLINGS)` DAA scores long, so the first
    /// `REWARD_INTERVAL_DOUBLINGS` periods double in length one after another and every later period is as long as the
    /// last of them. With `REWARD_INTERVAL_DOUBLINGS` 0 it is `⌊daa_score / REWARD_HALVING_INTERVAL⌋`. `None` when
    /// `REWARD_HALVING_INTERVAL` is 0 (no reward).
    pub fn reward_period_at(&self, daa_score: u64) -> Option<u64> {
        let mut len = u128::from(self.reward_halving_interval);
        if len == 0 {
            return None;
        }
        // The periods that double: after k of them `rest` is `daa_score − REWARD_HALVING_INTERVAL × (2^k − 1)`, which
        // is below the next period's length by k = 64 at the latest, so this runs at most 65 times
        let mut rest = u128::from(daa_score);
        let mut period = 0u64;
        while period < u64::from(self.reward_interval_doublings) {
            let Some(after) = rest.checked_sub(len) else {
                return Some(period);
            };
            rest = after;
            len = len.saturating_mul(2);
            period = period.saturating_add(1);
        }
        // Every later period has the length reached
        let later = u64::try_from(rest.checked_div(len)?).unwrap_or(u64::MAX);
        Some(period.saturating_add(later))
    }

    /// The block reward of a block with DAA score `daa_score` (D-074, D-105): `BLOCK_REWARD >> k` for its halving
    /// period k ([`Params::reward_period_at`]), 0 when shifted right by 128 bits or more; no reward when
    /// `REWARD_HALVING_INTERVAL` is 0.
    pub fn block_reward_at(&self, daa_score: u64) -> u128 {
        self.reward_period_at(daa_score)
            .and_then(|period| u32::try_from(period).ok())
            .and_then(|shift| self.block_reward.checked_shr(shift))
            .unwrap_or(0)
    }

    /// The block reward of a block with DAA score `daa_score` and weight `weight` (D-084): it occupies the `weight`
    /// reward slots at DAA scores `daa_score − weight + 1 … daa_score` (an empty block has `EMPTY_BLOCK_DIFFICULTY` to
    /// `MAX_EMPTY_BLOCK_WEIGHT` of them depending on its streak, D-090; a non-empty block has 1), summing the slots per
    /// [`Params::block_reward_at`] (saturating), which naturally splits when a halving boundary falls among them. No
    /// reward when `weight` is 0.
    pub fn block_rewards_at(&self, daa_score: u64, weight: u64) -> u128 {
        (0..weight)
            .filter_map(|i| daa_score.checked_sub(i))
            .map(|slot| self.block_reward_at(slot))
            .fold(0u128, u128::saturating_add)
    }

    /// The part of the reward slot at DAA score `daa_score` that goes to the block share account (D-089): per the
    /// slot's own rules ([`Params::rules`]), `reward × BLOCK_SHARE_BPS / 10 000` (rounded down, a fraction above 100%
    /// counts as 100%) when the DAA score is below `BLOCK_SHARE_END`, otherwise 0.
    pub fn block_share_at(&self, daa_score: u64) -> u128 {
        let rules = self.rules(daa_score);
        if daa_score >= rules.block_share_end {
            return 0;
        }
        bps_of(self.block_reward_at(daa_score), rules.block_share_bps)
    }

    /// The part of the reward slot at DAA score `daa_score` that goes to the carrier (D-095): per the slot's own
    /// rules, `reward × CARRIER_BPS / 10 000` (rounded down), at most the remainder after deducting the block share
    /// (when the two fractions sum to more than 100% the carrier only gets what is left).
    pub fn block_carrier_at(&self, daa_score: u64) -> u128 {
        let rules = self.rules(daa_score);
        let reward = self.block_reward_at(daa_score);
        let share = self.block_share_at(daa_score);
        bps_of(reward, rules.carrier_bps).min(reward.saturating_sub(share))
    }

    /// The part of a block's path B sponsored fees `amount` that goes to the carrier (D-095): per the block's own
    /// rules (DAA score `daa_score`), `amount × CARRIER_FEE_BPS / 10 000` (rounded down); the rest goes to the
    /// producer.
    pub fn carrier_fee_at(&self, daa_score: u64, amount: u128) -> u128 {
        bps_of(amount, self.rules(daa_score).carrier_fee_bps)
    }

    /// The block reward of a block with DAA score `daa_score` and weight `weight` split into the three parts for the
    /// producer, the block share account and the carrier (D-089, D-095): the sum of [`Params::block_share_at`] over
    /// the slots goes to the block share account, the sum of [`Params::block_carrier_at`] to the carrier, the rest to
    /// the producer; the three add up to [`Params::block_rewards_at`] (saturating sums).
    pub fn block_rewards_split_at(&self, daa_score: u64, weight: u64) -> RewardSplit {
        (0..weight).filter_map(|i| daa_score.checked_sub(i)).fold(
            RewardSplit::default(),
            |split, slot| {
                let reward = self.block_reward_at(slot);
                let slot_share = self.block_share_at(slot);
                let slot_carrier = self.block_carrier_at(slot);
                RewardSplit {
                    producer: split.producer.saturating_add(
                        reward
                            .saturating_sub(slot_share)
                            .saturating_sub(slot_carrier),
                    ),
                    share: split.share.saturating_add(slot_share),
                    carrier: split.carrier.saturating_add(slot_carrier),
                }
            },
        )
    }

    /// The weight of the `streak`-th empty block of an empty-block streak (D-084, D-090):
    /// `min(N × 2^(streak − 1), N_MAX)`, with N = `EMPTY_BLOCK_DIFFICULTY` at least 1 and
    /// N_MAX = `MAX_EMPTY_BLOCK_WEIGHT` at least N; `streak` 0 is treated as 1, and an overflowing product gives N_MAX.
    pub fn empty_block_weight(&self, streak: u64) -> u64 {
        let base = self.empty_block_difficulty.max(1);
        let cap = self.max_empty_block_weight.max(base);
        let doublings = streak.saturating_sub(1);
        u32::try_from(doublings)
            .ok()
            .and_then(|shift| 1u64.checked_shl(shift))
            .and_then(|factor| base.checked_mul(factor))
            .map_or(cap, |weight| weight.min(cap))
    }

    /// Whether the header is an empty block: `entries_root` is the root of the empty entry list.
    pub fn is_empty_block(header: &crate::BlockHeader) -> bool {
        header.entries_root == crate::compact::empty_entries_root()
    }

    /// The empty-block streak of a header (D-090): the number of consecutive empty blocks on the selected chain up to
    /// it, which is 0 for a non-empty block and the selected parent's streak `selected_parent_streak` plus one
    /// (saturating) for an empty block. The genesis block's streak counts as 0.
    pub fn block_streak(&self, header: &crate::BlockHeader, selected_parent_streak: u64) -> u64 {
        if Self::is_empty_block(header) {
            selected_parent_streak.saturating_add(1)
        } else {
            0
        }
    }

    /// The weight of a header (D-084, D-090): 1 for a non-empty block, [`Self::empty_block_weight`] at its streak
    /// ([`Self::block_streak`]) for an empty block. It is the DAA score increment, the divisor of the effective PoW
    /// target and the number of reward slots. By this the genesis block is also an empty block (streak 1, weight N),
    /// but its DAA score is fixed at 0.
    pub fn block_weight(&self, header: &crate::BlockHeader, selected_parent_streak: u64) -> u64 {
        match self.block_streak(header, selected_parent_streak) {
            0 => 1,
            streak => self.empty_block_weight(streak),
        }
    }

    /// Whether `weight` is a possible weight of some block: 1, or the empty block weight of some streak (D-084, D-090).
    /// Any other weight in the unlockable segment is a node / indexer fault. The weight reaches the limit after at
    /// most 64 doublings, so only streaks 1 to 65 are checked.
    pub fn is_block_weight(&self, weight: u64) -> bool {
        weight == 1 || (1..=65u64).any(|streak| self.empty_block_weight(streak) == weight)
    }

    /// An upper bound on the sum of all block rewards (D-074, D-105): when each DAA score corresponds to at most one
    /// rewarded block, `Σ_d reward(d) ≤ (REWARD_INTERVAL_DOUBLINGS + 2) × BLOCK_REWARD × REWARD_HALVING_INTERVAL`
    /// (saturating): each of the first `REWARD_INTERVAL_DOUBLINGS` halving periods pays at most
    /// `BLOCK_REWARD × REWARD_HALVING_INTERVAL` (the reward halves as the length doubles), and the periods of equal
    /// length after them at most twice that together. An empty block of weight w occupies the reward slots of w DAA
    /// scores (D-084, D-090), leaving the bound unchanged.
    pub fn max_total_rewards(&self) -> u128 {
        self.block_reward
            .saturating_mul(u128::from(self.reward_halving_interval))
            .saturating_mul(u128::from(self.reward_interval_doublings).saturating_add(2))
    }

    /// The limit on the total genesis allocation (D-074): `KSA_MAX_SUPPLY − upper bound on the total rewards`.
    pub fn max_genesis_supply(&self) -> u128 {
        self.ksa_max_supply.saturating_sub(self.max_total_rewards())
    }

    /// The rules that apply to a block with DAA score `daa_score` (D-076): starting from the base rules (the values in
    /// the params, protocol version [`crate::BLOCK_VERSION`], VmOp version [`crate::VMOP_VERSION`]), the upgrades whose
    /// `daa_score` does not exceed it are applied in turn.
    pub fn rules(&self, daa_score: u64) -> Rules {
        let base = Rules {
            version: crate::BLOCK_VERSION,
            block_gas_limit: self.block_gas_limit,
            native_max_gas: self.native_max_gas,
            min_vmop_version: crate::VMOP_VERSION,
            max_vmop_version: crate::VMOP_VERSION,
            block_share_bps: self.block_share_bps,
            block_share_end: self.block_share_end,
            carrier_bps: self.carrier_bps,
            carrier_fee_bps: self.carrier_fee_bps,
        };
        self.upgrades
            .iter()
            .filter(|u| u.daa_score <= daa_score)
            .fold(base, |rules, u| Rules {
                version: u.version,
                block_gas_limit: u.block_gas_limit.unwrap_or(rules.block_gas_limit),
                native_max_gas: u.native_max_gas.unwrap_or(rules.native_max_gas),
                min_vmop_version: u.min_vmop_version.unwrap_or(rules.min_vmop_version),
                max_vmop_version: u.max_vmop_version.unwrap_or(rules.max_vmop_version),
                block_share_bps: u.block_share_bps.unwrap_or(rules.block_share_bps),
                block_share_end: u.block_share_end.unwrap_or(rules.block_share_end),
                carrier_bps: u.carrier_bps.unwrap_or(rules.carrier_bps),
                carrier_fee_bps: u.carrier_fee_bps.unwrap_or(rules.carrier_fee_bps),
            })
    }

    /// The protocol version (block header version, D-076) of a block with DAA score `daa_score`.
    pub fn protocol_version(&self, daa_score: u64) -> u32 {
        self.rules(daa_score).version
    }

    /// The next scheduled upgrade after a block with DAA score `daa_score`.
    pub fn next_upgrade(&self, daa_score: u64) -> Option<&Upgrade> {
        self.upgrades.iter().find(|u| u.daa_score > daa_score)
    }

    /// A one-line description of the upgrade table (printed by the node and the indexer at startup, D-076).
    pub fn upgrade_schedule(&self) -> String {
        if self.upgrades.is_empty() {
            return format!(
                "protocol version {}, no scheduled upgrades",
                crate::BLOCK_VERSION
            );
        }
        let items: Vec<String> = self
            .upgrades
            .iter()
            .map(|u| format!("version {} from DAA score {}", u.version, u.daa_score))
            .collect();
        format!(
            "base version {}; {}",
            crate::BLOCK_VERSION,
            items.join("; ")
        )
    }

    /// The highest protocol version this software knows: a block with a higher header version belongs to an upgrade
    /// this software does not know (D-076).
    pub fn max_known_version(&self) -> u32 {
        self.upgrades
            .last()
            .map_or(crate::BLOCK_VERSION, |u| u.version)
    }

    /// Check the upgrade table (D-076): `version` and `daa_score` strictly increasing, the first item's version greater
    /// than [`crate::BLOCK_VERSION`] and its DAA score at least 1; in the rules in force after each item the gas limits
    /// are greater than 0, `NATIVE_MAX_GAS` does not exceed `BLOCK_GAS_LIMIT`, the VmOp version range is non-empty, the
    /// fractions of the block share and the carrier reward sum to at most 100%, and the carrier's sponsored-fee
    /// fraction is at most 100% (D-089, D-095).
    pub fn check_upgrades(&self) -> Result<(), String> {
        let mut version = crate::BLOCK_VERSION;
        let mut daa_score = 0u64;
        for u in self.upgrades {
            if u.version <= version {
                return Err(format!(
                    "upgrade protocol version {} is not greater than the previous {version}",
                    u.version
                ));
            }
            if u.daa_score <= daa_score {
                return Err(format!(
                    "the activation point of version {}, DAA score {}, is not greater than the previous {daa_score}",
                    u.version, u.daa_score
                ));
            }
            let rules = self.rules(u.daa_score);
            if rules.block_gas_limit == 0
                || rules.native_max_gas == 0
                || rules.native_max_gas > rules.block_gas_limit
            {
                return Err(format!(
                    "the gas limits of version {} are unreasonable",
                    u.version
                ));
            }
            if rules.min_vmop_version > rules.max_vmop_version {
                return Err(format!(
                    "the VmOp version range of version {} is empty",
                    u.version
                ));
            }
            if rules.block_share_bps > 10_000 {
                return Err(format!(
                    "the block share fraction of version {} exceeds 100%",
                    u.version
                ));
            }
            if rules
                .block_share_bps
                .checked_add(rules.carrier_bps)
                .is_none_or(|sum| sum > 10_000)
            {
                return Err(format!(
                    "the block share and carrier reward fractions of version {} sum to more than 100% (D-095)",
                    u.version
                ));
            }
            if rules.carrier_fee_bps > 10_000 {
                return Err(format!(
                    "the carrier sponsored-fee fraction of version {} exceeds 100% (D-095)",
                    u.version
                ));
            }
            version = u.version;
            daa_score = u.daa_score;
        }
        Ok(())
    }
}

const KIB: usize = 1024;
const MIB: u64 = 1024 * 1024;

/// The devnet default parameters (the SPEC §13 table).
pub const DEVNET: Params = Params {
    chain_id: 1001,
    magic: *b"KSN1",
    target_block_time_secs: 5,
    // 7 200 seconds (D-090): scaled up with the empty block weight limit: at the limit each empty block takes about
    // 600 seconds, so half-life / time per block is the same as D-084's 600 seconds / 60 seconds, and the difficulty
    // jitter when idle is unchanged (about ±20%)
    asert_half_life_secs: 7200,
    settlement_depth: 2,
    settlement_timeout: 6,
    native_confirmations: 1,
    max_anchor_lag: 1,
    anchor_depth: 0,
    btc_query_depth: 6,
    btc_finality_confs: 6,
    min_settlement_feerate_sat_vb: 1,
    block_gas_limit: 100_000_000,
    native_max_gas: 1_000_000,
    max_op_bytes: 64 * KIB,
    max_block_data_bytes: 80 * KIB,
    // Raised from 100 once the headers in the payload were compressed (D-075)
    max_settlement_headers: 500,
    max_wasm_memory_bytes: 16 * MIB,
    max_call_depth: 8,
    reorg_window: 1000,
    median_time_span: 11,
    max_future_block_time_secs: 15,
    // 4 hours (D-083): the BTC block header time itself may be 2 hours ahead, plus a margin for the anchor lagging
    // behind the tip and for block production
    max_timestamp_ahead_of_anchor_secs: 4 * 3600,
    pow_limit_bits: 0x207f_ffff,
    // Argon2id (D-094): 2 MiB, 1 pass, 1 lane: about 1 millisecond per header on a single thread of an ordinary CPU,
    // and a browser can verify a single header
    pow_algorithm: PowAlgorithmId::Argon2id(Argon2Params {
        memory_kib: 2048,
        iterations: 1,
        lanes: 1,
    }),
    // 16 rather than D-063's 8 (D-100): the honest weight-1 blocks that a withheld empty block of weight
    // EMPTY_BLOCK_DIFFICULTY (12) can overtake then fit in a blue anticone, so on a busy chain withholding one no
    // longer turns them red; the propagation delay K tolerates (δ = 10⁻³) grows from about 6 to about 17 seconds
    ghostdag_k: 16,
    max_block_parents: 10,
    max_mergeset_size: 128,
    merge_depth: 720,
    // Placeholder values of D-074: 9.9 KSA (precision 8), halving every 1 460 days (one block per 5 seconds; the
    // halving periods do not double, D-105), supply cap of 1 billion KSA
    block_reward: 990_000_000,
    reward_halving_interval: 25_228_800,
    reward_interval_doublings: 0,
    ksa_max_supply: 100_000_000_000_000_000,
    max_unlock_batch: 1024,
    // Block share (D-089): for the first two halving periods (2 × REWARD_HALVING_INTERVAL), 20% of each reward slot is
    // credited to the block share account of the genesis config
    block_share_bps: 2_000,
    block_share_end: 50_457_600,
    // Carrier reward (D-095): 5% of each reward slot is taken out of the producer's share and given to the producer
    // who lands that block's data on BTC (block share unchanged), path B sponsored fees are split in half; the payee
    // of a republish payload only qualifies once the block's data waited more than ANCHOR_DEPTH + 2 BTC blocks after
    // its anchor
    carrier_bps: 500,
    carrier_fee_bps: 5_000,
    carrier_repost_delay: 2,
    // Two-speed difficulty (D-084): the first empty block after a non-empty block is 12 times harder, weight 12,
    // 12 reward slots; the weight of consecutive empty blocks doubles up to 120 (D-090): 12, 24, 48, 96, 120, 120...,
    // so when idle the interval between headers landing on BTC doubles from about 60 seconds to about 10 minutes
    empty_block_difficulty: 12,
    max_empty_block_weight: 120,
    // No scheduled upgrades (D-076)
    upgrades: &[],
};

/// The signet parameters (D-055, the M6 browser wallet acceptance): differ from [`DEVNET`] only in `chain_id`
/// (preventing replay between devnet and signet), `ANCHOR_DEPTH` (signet has real BTC reorgs, D-056) and
/// `pow_limit_bits` 0x1f0fffff (a target of about 2^244, about 4 096 Argon2id evaluations per block at the limit, so
/// that a side branch whose difficulty fell to the limit still pays for each header it lands on BTC, D-136).
pub const SIGNET: Params = Params {
    chain_id: 1002,
    anchor_depth: 1,
    pow_limit_bits: 0x1f0f_ffff,
    ..DEVNET
};

/// Mainnet parameters (the mainnet column of SPEC §13; the rationale, and which values are only defaults, is in SPEC
/// §13.1; D-096). Differences from [`DEVNET`]:
/// - `chain_id` 1000 (devnet 1001, signet 1002);
/// - `NATIVE_CONFIRMATIONS` 3 and `ANCHOR_DEPTH` 2 (the SPEC §13 recommendation for mainnet, D-036, D-056: a path C op
///   is included about 5 BTC blocks after its carrying transaction, and the anchor trails the tip by 2 blocks to avoid
///   the most common 1-block reorg);
/// - `pow_limit_bits` 0x1f00ffff (a target of about 2^240, i.e. an expected 65 536 Argon2id evaluations per block:
///   about 1 ms each on one thread, so at the limit about 13 cores suffice for one block per 5 seconds; the difficulty
///   only adjusts upward from here, so the genesis config should set `bits` near the expected hashrate, D-027);
/// - KSA issuance (D-089, D-105): a max supply of 210 million KSA, 4.2 KSA per reward slot, halving periods of
///   12 500 000, 25 000 000 and then 50 000 000 DAA score each (`REWARD_INTERVAL_DOUBLINGS` 2: about 2, 4 and then 8
///   years at one block per 5 seconds, so the halvings fall in about years 2, 6, 14, 22, 30, …); the upper bound on
///   total rewards is exactly the max supply, so mainnet has no genesis allocation ([`Params::max_genesis_supply`] is
///   0);
/// - the block share (D-089) is not published yet (D-134): `block_share_bps` and `block_share_end` hold placeholders
///   (0, no share), and a mainnet node or indexer refuses to start until they are filled in
///   (`GenesisFile::check_network` of `ksana-execution`).
///
/// All other parameters are the same as devnet.
pub const MAINNET: Params = Params {
    chain_id: 1000,
    native_confirmations: 3,
    anchor_depth: 2,
    pow_limit_bits: 0x1f00_ffff,
    block_reward: 420_000_000,
    reward_halving_interval: 12_500_000,
    reward_interval_doublings: 2,
    ksa_max_supply: 21_000_000_000_000_000,
    // The block share (D-089) is not published yet (D-134): placeholders, and mainnet does not start until they are
    // filled in
    block_share_bps: 0,
    block_share_end: 0,
    ..DEVNET
};

#[cfg(test)]
mod tests {
    use super::*;

    /// signet differs from devnet only in `chain_id` (D-055), `ANCHOR_DEPTH` (D-056) and the PoW limit (D-136).
    #[test]
    fn signet_differs_from_devnet_only_in_chain_id_anchor_depth_and_pow_limit() {
        let signet = Network::Signet.params();
        assert_eq!(signet.chain_id, 1002);
        assert_eq!(signet.anchor_depth, 1);
        assert_eq!(signet.pow_limit_bits, 0x1f0f_ffff);
        assert_eq!(DEVNET.anchor_depth, 0);
        assert_eq!(
            Params {
                chain_id: DEVNET.chain_id,
                anchor_depth: DEVNET.anchor_depth,
                pow_limit_bits: DEVNET.pow_limit_bits,
                ..signet.clone()
            },
            DEVNET
        );
        assert_eq!(Network::from_name("signet"), Some(Network::Signet));
        assert_eq!(Network::from_name("devnet"), Some(Network::Devnet));
        assert_eq!(Network::from_name("Devnet"), None);
        assert_eq!(Network::from_name(""), None);
    }

    /// Mainnet (D-096): chain ID 1000, `NATIVE_CONFIRMATIONS` 3 and `ANCHOR_DEPTH` 2 (SPEC §13), PoW limit 0x1f00ffff,
    /// KSA issuance per D-089 and D-105, placeholders for the block share (D-134); everything else as devnet. Both the
    /// name and the chain ID find the network back, and the three chain IDs are distinct.
    #[test]
    fn mainnet_matches_spec_table() {
        let p = Network::Mainnet.params();
        assert_eq!(p.chain_id, 1000);
        assert_eq!(p.native_confirmations, 3);
        assert_eq!(p.anchor_depth, 2);
        assert_eq!(p.pow_limit_bits, 0x1f00_ffff);
        assert_eq!(p.block_reward, 420_000_000);
        assert_eq!(p.reward_halving_interval, 12_500_000);
        assert_eq!(p.reward_interval_doublings, 2);
        assert_eq!(p.ksa_max_supply, 21_000_000_000_000_000);
        assert_eq!(p.block_share_bps, 0);
        assert_eq!(p.block_share_end, 0);
        assert_eq!(p.carrier_bps, 500);
        assert_eq!(p.carrier_fee_bps, 5_000);
        assert_eq!(p.carrier_repost_delay, 2);
        assert_eq!(
            Params {
                chain_id: DEVNET.chain_id,
                native_confirmations: DEVNET.native_confirmations,
                anchor_depth: DEVNET.anchor_depth,
                pow_limit_bits: DEVNET.pow_limit_bits,
                block_reward: DEVNET.block_reward,
                reward_halving_interval: DEVNET.reward_halving_interval,
                reward_interval_doublings: DEVNET.reward_interval_doublings,
                ksa_max_supply: DEVNET.ksa_max_supply,
                block_share_bps: DEVNET.block_share_bps,
                block_share_end: DEVNET.block_share_end,
                ..p.clone()
            },
            DEVNET
        );
        assert!(p.check_upgrades().is_ok());
        assert_eq!(Network::from_name("mainnet"), Some(Network::Mainnet));
        for n in Network::ALL {
            assert_eq!(Network::from_name(n.name()), Some(n));
            assert_eq!(Network::from_chain_id(n.params().chain_id), Some(n));
        }
        assert_eq!(Network::from_chain_id(0), None);
        assert_eq!(Network::from_chain_id(1003), None);
    }

    /// Mainnet issuance of D-089 and D-105: the upper bound on total rewards is exactly the max supply of 210 million
    /// KSA (no room for a genesis allocation); the halving periods are 12 500 000, 25 000 000 and then 50 000 000 DAA
    /// score long; the exact sum over the periods is exactly 4 KSA short of the cap (from the third period on the
    /// periods are all 50 000 000 long, and at each position their rewards sum to 2 × 105 000 000 − 8 base units:
    /// 105 000 000 has 8 one-bits in binary, and the first shift that truncates is in period 9; times the 50 000 000
    /// positions that is 4 KSA); the carrier's part of a 4.2 KSA slot is 0.21; the last base unit is paid in period 28,
    /// and from DAA score 1 387 500 000 (about year 220) the reward is 0.
    #[test]
    fn mainnet_rewards_fill_the_cap() {
        let p = &MAINNET;
        assert_eq!(p.max_total_rewards(), p.ksa_max_supply);
        assert_eq!(p.max_genesis_supply(), 0);
        let total = exact_total_rewards(p);
        assert_eq!(total, 20_999_999_600_000_000);
        assert_eq!(p.ksa_max_supply - total, 400_000_000);
        // The period boundaries: 12 500 000, 37 500 000, 87 500 000, then every 50 000 000
        for (daa, period, reward) in [
            (0, 0, 420_000_000),
            (12_499_999, 0, 420_000_000),
            (12_500_000, 1, 210_000_000),
            (37_499_999, 1, 210_000_000),
            (37_500_000, 2, 105_000_000),
            (87_499_999, 2, 105_000_000),
            (87_500_000, 3, 52_500_000),
            (137_499_999, 3, 52_500_000),
            (137_500_000, 4, 26_250_000),
            (1_387_499_999, 28, 1),
            (1_387_500_000, 29, 0),
        ] {
            assert_eq!(p.reward_period_at(daa), Some(period), "{daa}");
            assert_eq!(p.block_reward_at(daa), reward, "{daa}");
        }
        assert_eq!(p.block_reward_at(u64::MAX), 0);
        assert_eq!(p.block_carrier_at(0), 21_000_000);
        assert_eq!(p.block_carrier_at(12_500_000), 10_500_000);
    }

    /// The exact sum of all reward slots, period by period (D-105): period k has
    /// `REWARD_HALVING_INTERVAL << min(k, REWARD_INTERVAL_DOUBLINGS)` slots of `BLOCK_REWARD >> k` each.
    fn exact_total_rewards(p: &Params) -> u128 {
        (0..128u32)
            .map(|k| {
                let len =
                    u128::from(p.reward_halving_interval) << k.min(p.reward_interval_doublings);
                (p.block_reward >> k) * len
            })
            .sum()
    }

    /// Check the devnet defaults of SPEC §13 item by item.
    #[test]
    fn devnet_matches_spec_table() {
        let p = Network::Devnet.params();
        assert_eq!(p.chain_id, 1001);
        assert_eq!(&p.magic, b"KSN1");
        assert_eq!(p.target_block_time_secs, 5);
        assert_eq!(p.asert_half_life_secs, 7200); // D-090
        assert_eq!(p.settlement_depth, 2);
        assert_eq!(p.settlement_timeout, 6);
        assert_eq!(p.native_confirmations, 1);
        assert_eq!(p.max_anchor_lag, 1);
        assert_eq!(p.btc_query_depth, 6);
        assert_eq!(p.btc_finality_confs, 6);
        assert_eq!(p.min_settlement_feerate_sat_vb, 1);
        assert_eq!(p.block_gas_limit, 100_000_000);
        assert_eq!(p.native_max_gas, 1_000_000);
        assert_eq!(p.max_op_bytes, 65_536);
        assert_eq!(p.max_block_data_bytes, 81_920);
        assert_eq!(p.max_settlement_headers, 500); // D-075
        assert_eq!(p.max_wasm_memory_bytes, 16_777_216);
        assert_eq!(p.max_call_depth, 8);
        assert_eq!(p.reorg_window, 1000);
        assert_eq!(p.median_time_span, 11);
        assert_eq!(p.max_future_block_time_secs, 15);
        // Not in the spec table (D-083): the timestamp limit relative to the anchored BTC block time
        assert_eq!(p.max_timestamp_ahead_of_anchor_secs, 14_400);
        // Not in the spec table (D-025): the same minimum difficulty as BTC regtest
        assert_eq!(p.pow_limit_bits, 0x207f_ffff);
        // Mining algorithm (D-062, D-094): Argon2id, 2 MiB, 1 pass, 1 lane
        assert_eq!(
            p.pow_algorithm,
            PowAlgorithmId::Argon2id(Argon2Params {
                memory_kib: 2048,
                iterations: 1,
                lanes: 1,
            })
        );
        assert_eq!(Argon2Params::OUTPUT_LEN, 32);
        assert_eq!(
            Argon2Params::salt(p.chain_id).to_vec(),
            [b"ksana-pow".as_slice(), &[0xe9, 0x03, 0, 0]].concat()
        );
        // Not in the spec table (D-063): GHOSTDAG
        assert_eq!(p.ghostdag_k, 16);
        assert_eq!(p.max_block_parents, 10);
        assert_eq!(p.max_mergeset_size, 128);
        assert_eq!(p.merge_depth, 720);
        // Not in the spec table (D-074): block rewards
        assert_eq!(p.block_reward, 990_000_000);
        assert_eq!(p.reward_halving_interval, 25_228_800);
        assert_eq!(p.reward_interval_doublings, 0); // D-105
        assert_eq!(p.ksa_max_supply, 100_000_000_000_000_000);
        // Not in the spec table (D-089): block share
        assert_eq!(p.block_share_bps, 2_000);
        assert_eq!(p.block_share_end, 2 * p.reward_halving_interval);
        // Not in the spec table (D-084, D-090): two-speed difficulty and the empty block weight limit
        assert_eq!(p.empty_block_difficulty, 12);
        assert_eq!(p.max_empty_block_weight, 120);
    }

    /// D-084 / D-090: a non-empty block has weight 1; an empty block's weight doubles block by block with the streak
    /// from `EMPTY_BLOCK_DIFFICULTY` to `MAX_EMPTY_BLOCK_WEIGHT`; a block of weight w gets the sum of w reward slots,
    /// split when a halving boundary falls among them.
    #[test]
    fn empty_blocks_weigh_by_streak_and_take_as_many_reward_slots() {
        let header = crate::BlockHeader {
            version: crate::BLOCK_VERSION,
            height: 1,
            parents: vec![[1; 32]],
            btc_anchor: crate::BtcAnchor {
                height: 1,
                hash: [2; 32],
            },
            entries_root: crate::compact::empty_entries_root(),
            state_root: [3; 32],
            producer: [4; 32],
            timestamp: 1,
            bits: DEVNET.pow_limit_bits,
            nonce: 0,
        };
        assert!(Params::is_empty_block(&header));
        // The first empty block after a non-empty block (selected parent streak 0) is 12, then 24, 48, 96, up to the
        // limit 120
        for (sp_streak, weight) in [
            (0, 12),
            (1, 24),
            (2, 48),
            (3, 96),
            (4, 120),
            (5, 120),
            (100, 120),
        ] {
            assert_eq!(
                DEVNET.block_weight(&header, sp_streak),
                weight,
                "after streak {sp_streak}"
            );
            assert_eq!(DEVNET.block_streak(&header, sp_streak), sp_streak + 1);
        }
        assert_eq!(
            DEVNET.block_streak(&header, u64::MAX),
            u64::MAX,
            "saturating"
        );
        assert_eq!(DEVNET.empty_block_weight(0), 12, "streak 0 is treated as 1");
        let non_empty = crate::BlockHeader {
            entries_root: [5; 32],
            ..header.clone()
        };
        assert!(!Params::is_empty_block(&non_empty));
        for sp_streak in [0, 1, 7, u64::MAX] {
            assert_eq!(DEVNET.block_weight(&non_empty, sp_streak), 1);
            assert_eq!(
                DEVNET.block_streak(&non_empty, sp_streak),
                0,
                "a non-empty block ends the streak"
            );
        }
        let zero = Params {
            empty_block_difficulty: 0,
            max_empty_block_weight: 0,
            ..DEVNET
        };
        assert_eq!(zero.block_weight(&header, 0), 1, "0 is treated as 1");
        assert_eq!(zero.block_weight(&header, 9), 1, "limit 0 is treated as N");
        let low_cap = Params {
            max_empty_block_weight: 5,
            ..DEVNET
        };
        assert_eq!(
            low_cap.empty_block_weight(1),
            12,
            "a limit below N is treated as N"
        );
        assert_eq!(low_cap.empty_block_weight(2), 12);
        let huge = Params {
            empty_block_difficulty: 1 << 62,
            max_empty_block_weight: u64::MAX,
            ..DEVNET
        };
        assert_eq!(huge.empty_block_weight(2), 1 << 63);
        assert_eq!(
            huge.empty_block_weight(3),
            u64::MAX,
            "the limit on overflow"
        );
        assert_eq!(huge.empty_block_weight(u64::MAX), u64::MAX);
        // Possible weights: 1 and the empty block weight of each streak
        for weight in [1, 12, 24, 48, 96, 120] {
            assert!(DEVNET.is_block_weight(weight), "{weight}");
        }
        for weight in [0, 2, 5, 11, 13, 60, 119, 121, 192, 240, u64::MAX] {
            assert!(!DEVNET.is_block_weight(weight), "{weight}");
        }
        assert!(huge.is_block_weight(u64::MAX) && !huge.is_block_weight(2));

        let p = Params {
            block_reward: 1_000,
            reward_halving_interval: 20,
            ..DEVNET
        };
        assert_eq!(p.block_rewards_at(7, 1), 1_000);
        assert_eq!(p.block_rewards_at(20, 1), 500);
        // Slots 13-24: 1 000 each for 13-19, 500 each for 20-24
        assert_eq!(p.block_rewards_at(24, 12), 7 * 1_000 + 5 * 500);
        assert_eq!(p.block_rewards_at(12, 12), 12 * 1_000);
        assert_eq!(p.block_rewards_at(5, 0), 0);
        // Slots never go below 0 (the genesis block has DAA score 0 and weight 1)
        assert_eq!(p.block_rewards_at(0, 1), 1_000);
        assert_eq!(p.block_rewards_at(1, 12), 2 * 1_000, "only slots 0 and 1");
        let huge = Params {
            block_reward: u128::MAX,
            reward_halving_interval: 1 << 40,
            ..DEVNET
        };
        assert_eq!(huge.block_rewards_at(100, 12), u128::MAX, "saturating");
    }

    /// The reward schedule (D-074): constant within each halving interval, halves at the interval boundary, 0 when
    /// shifted right by 128 bits or more.
    #[test]
    fn block_reward_halves_on_schedule() {
        let p = DEVNET;
        let h = p.reward_halving_interval;
        assert_eq!(p.block_reward_at(0), 990_000_000);
        assert_eq!(p.block_reward_at(h - 1), 990_000_000);
        assert_eq!(p.block_reward_at(h), 495_000_000);
        assert_eq!(p.block_reward_at(2 * h), 247_500_000);
        assert_eq!(p.block_reward_at(3 * h - 1), 247_500_000);
        assert_eq!(p.block_reward_at(30 * h), 0, "990 000 000 < 2^30");
        assert_eq!(p.block_reward_at(u64::MAX), 0);
        let none = Params {
            reward_halving_interval: 0,
            ..DEVNET
        };
        assert_eq!(none.block_reward_at(0), 0);
        let huge = Params {
            block_reward: u128::MAX,
            reward_halving_interval: 1,
            ..DEVNET
        };
        assert_eq!(huge.block_reward_at(127), 1);
        assert_eq!(huge.block_reward_at(128), 0);
    }

    /// D-105: the halving periods double in length `REWARD_INTERVAL_DOUBLINGS` times and then keep the last length; the
    /// reward halves at every period boundary; 0 doublings is D-074's schedule; any number of doublings neither
    /// overflows nor loops for long; the exact sum stays under `(doublings + 2) × BLOCK_REWARD × interval`.
    #[test]
    fn halving_periods_double_then_stay() {
        let p = Params {
            block_reward: 1_000,
            reward_halving_interval: 3,
            reward_interval_doublings: 2,
            ..DEVNET
        };
        // Periods 3, 6, 12, 12, … long: boundaries at 3, 9, 21, 33, 45, …
        for (daa, period) in [
            (0, 0),
            (2, 0),
            (3, 1),
            (8, 1),
            (9, 2),
            (20, 2),
            (21, 3),
            (32, 3),
            (33, 4),
            (45, 5),
        ] {
            assert_eq!(p.reward_period_at(daa), Some(period), "{daa}");
            assert_eq!(p.block_reward_at(daa), 1_000 >> period, "{daa}");
        }
        assert_eq!(p.reward_period_at(u64::MAX), Some((u64::MAX - 9) / 12 + 2));
        assert_eq!(p.block_reward_at(u64::MAX), 0);
        let flat = Params {
            reward_interval_doublings: 0,
            ..p.clone()
        };
        for daa in [0, 2, 3, 8, 9, 20, 21, 1_000, u64::MAX] {
            assert_eq!(flat.reward_period_at(daa), Some(daa / 3), "{daa}");
        }
        let none = Params {
            reward_halving_interval: 0,
            ..p.clone()
        };
        assert_eq!(none.reward_period_at(5), None);
        assert_eq!(none.block_reward_at(5), 0);
        assert_eq!(none.max_total_rewards(), 0);
        // Doubling without end: with interval 1 period k starts at 2^k − 1, so the largest DAA score starts period 64
        let endless = Params {
            block_reward: u128::MAX,
            reward_halving_interval: 1,
            reward_interval_doublings: u32::MAX,
            ..DEVNET
        };
        for (daa, period) in [
            (0, 0),
            (1, 1),
            (2, 1),
            (3, 2),
            (u64::MAX - 1, 63),
            (u64::MAX, 64),
        ] {
            assert_eq!(endless.reward_period_at(daa), Some(period), "{daa}");
        }
        assert_eq!(endless.block_reward_at(u64::MAX), u128::MAX >> 64);
        assert_eq!(endless.max_total_rewards(), u128::MAX, "saturating");
        let widest = Params {
            reward_halving_interval: u64::MAX,
            ..endless.clone()
        };
        assert_eq!(widest.reward_period_at(u64::MAX - 1), Some(0));
        assert_eq!(widest.reward_period_at(u64::MAX), Some(1));
        // The bound and the exact sum
        assert_eq!(p.max_total_rewards(), 4 * 1_000 * 3);
        assert_eq!(
            exact_total_rewards(&p),
            3 * 1_000 + 6 * 500 + 12 * (250 + 125 + 62 + 31 + 15 + 7 + 3 + 1)
        );
        assert!(exact_total_rewards(&p) <= p.max_total_rewards());
    }

    /// The total rewards (summed exactly period by period) do not exceed the upper bound, and the bound plus the
    /// genesis allocation limit equals the supply cap.
    #[test]
    fn total_rewards_stay_under_the_cap() {
        for p in [DEVNET, SIGNET] {
            let total = exact_total_rewards(&p);
            assert!(total <= p.max_total_rewards(), "{total}");
            assert_eq!(
                p.max_genesis_supply() + p.max_total_rewards(),
                p.ksa_max_supply
            );
            // Placeholder values: the rewards are about half of the cap
            assert!(p.max_total_rewards() <= p.ksa_max_supply / 2);
            assert!(p.max_total_rewards() > p.ksa_max_supply / 2 * 99 / 100);
        }
    }

    /// D-089: the block share is per slot: slots with DAA score below `BLOCK_SHARE_END` give `BLOCK_SHARE_BPS / 10 000`
    /// (rounded down) and the rest goes to the producer; later slots go entirely to the producer; the split sums to
    /// `block_rewards_at`; the fraction and the end point vary with the upgrade table, and the fraction cannot exceed
    /// 100%.
    #[test]
    fn block_share_splits_each_slot_until_block_share_end() {
        let p = Params {
            block_reward: 1_001,
            reward_halving_interval: 10,
            block_share_bps: 2_000,
            block_share_end: 20,
            carrier_bps: 0,
            ..DEVNET
        };
        assert_eq!(p.block_share_at(0), 200, "1 001 × 20% rounded down");
        assert_eq!(p.block_share_at(9), 200);
        assert_eq!(p.block_share_at(10), 100, "500 × 20% after the halving");
        assert_eq!(p.block_share_at(19), 100);
        assert_eq!(p.block_share_at(20), 0, "share ended");
        assert_eq!(p.block_share_at(u64::MAX), 0);
        assert_eq!(p.block_rewards_split_at(5, 1), split(801, 200, 0));
        // Weight 12, DAA score 25: slots 14-19 are 500 each (share 100), slots 20-25 are 250 each (no share)
        assert_eq!(
            p.block_rewards_split_at(25, 12),
            split(6 * 400 + 6 * 250, 600, 0)
        );
        assert_eq!(
            p.block_rewards_split_at(25, 12).total(),
            p.block_rewards_at(25, 12)
        );
        assert_eq!(p.block_rewards_split_at(5, 0), split(0, 0, 0));
        assert_eq!(
            p.block_rewards_split_at(1, 12),
            split(2 * 801, 2 * 200, 0),
            "only slots 0 and 1"
        );
        // Fractions 100% and 0%, above 100% counts as 100%
        let all = Params {
            block_share_bps: 10_000,
            ..p.clone()
        };
        assert_eq!(all.block_rewards_split_at(0, 1), split(0, 1_001, 0));
        let none = Params {
            block_share_bps: 0,
            ..p.clone()
        };
        assert_eq!(none.block_rewards_split_at(0, 1), split(1_001, 0, 0));
        let over = Params {
            block_share_bps: 20_000,
            ..p.clone()
        };
        assert_eq!(over.block_share_at(0), 1_001);
        // Even the largest reward does not overflow
        let huge = Params {
            block_reward: u128::MAX,
            reward_halving_interval: 1 << 40,
            block_share_end: u64::MAX,
            carrier_bps: 0,
            ..DEVNET
        };
        assert_eq!(
            huge.block_share_at(0),
            u128::MAX / 10_000 * 2_000 + (u128::MAX % 10_000) * 2_000 / 10_000
        );
        assert_eq!(
            huge.block_rewards_split_at(100, 12),
            split(u128::MAX, u128::MAX, 0),
            "saturating"
        );
        // Upgrade table: from version 3 (activation point 15) the fraction becomes 10% and the end point moves to 40
        const UPGRADES: &[Upgrade] = &[Upgrade {
            block_share_bps: Some(1_000),
            block_share_end: Some(40),
            ..Upgrade::new(3, 15)
        }];
        let q = Params {
            upgrades: UPGRADES,
            ..p
        };
        assert_eq!(q.check_upgrades(), Ok(()));
        assert_eq!(q.block_share_at(14), 100, "still 20% before activation");
        assert_eq!(q.block_share_at(15), 50, "10% after activation");
        assert_eq!(
            q.block_share_at(39),
            12,
            "125 × 10% rounded down, end point moved later"
        );
        assert_eq!(q.block_share_at(40), 0);
        assert_eq!(
            q.block_rewards_split_at(16, 3),
            split(400 + 2 * 450, 100 + 2 * 50, 0)
        );
        const TOO_MUCH: &[Upgrade] = &[Upgrade {
            block_share_bps: Some(10_001),
            ..Upgrade::new(3, 15)
        }];
        assert!(Params {
            upgrades: TOO_MUCH,
            ..DEVNET
        }
        .check_upgrades()
        .is_err());
    }

    fn split(producer: u128, share: u128, carrier: u128) -> RewardSplit {
        RewardSplit {
            producer,
            share,
            carrier,
        }
    }

    /// D-095: the carrier reward is per slot: `CARRIER_BPS / 10 000` (rounded down) of each slot comes out of the
    /// producer's share, the block share unchanged; the three parts sum to `block_rewards_at`; the sponsored fees are
    /// split by `CARRIER_FEE_BPS`; both fractions vary with the upgrade table, the share and carrier fractions cannot
    /// sum to more than 100%, and the fee fraction cannot exceed 100%; when the sum exceeds 100% the carrier only gets
    /// what is left after the share.
    #[test]
    fn carrier_takes_a_share_of_each_slot_and_of_the_fees() {
        let p = Params {
            block_reward: 1_001,
            reward_halving_interval: 10,
            block_share_bps: 2_000,
            block_share_end: 20,
            carrier_bps: 500,
            carrier_fee_bps: 5_000,
            ..DEVNET
        };
        assert_eq!(p.block_carrier_at(0), 50, "1 001 × 5% rounded down");
        assert_eq!(p.block_carrier_at(10), 25, "500 × 5% after the halving");
        assert_eq!(
            p.block_carrier_at(20),
            12,
            "the carrier reward continues after the share ended: 250 × 5%"
        );
        assert_eq!(p.block_rewards_split_at(5, 1), split(751, 200, 50));
        // Weight 12, DAA score 25: slots 14-19 are 500 each (share 100, carrier 25), slots 20-25 are 250 each
        // (carrier 12)
        assert_eq!(
            p.block_rewards_split_at(25, 12),
            split(6 * 375 + 6 * 238, 600, 6 * 25 + 6 * 12)
        );
        assert_eq!(
            p.block_rewards_split_at(25, 12).total(),
            p.block_rewards_at(25, 12)
        );
        assert_eq!(
            p.carrier_fee_at(0, 1_001),
            500,
            "sponsored fees split in half, rounded down"
        );
        assert_eq!(p.carrier_fee_at(0, 1), 0);
        assert_eq!(p.carrier_fee_at(0, u128::MAX), u128::MAX / 2);
        // The two fractions sum to more than 100%: the carrier only gets what is left after the share, and the upgrade
        // table rejects such rules
        let over = Params {
            block_share_bps: 9_800,
            ..p.clone()
        };
        assert_eq!(over.block_carrier_at(0), 1_001 - 980);
        assert_eq!(over.block_rewards_split_at(0, 1), split(0, 980, 21));
        const TOO_MUCH: &[Upgrade] = &[Upgrade {
            carrier_bps: Some(8_001),
            ..Upgrade::new(3, 15)
        }];
        assert!(Params {
            upgrades: TOO_MUCH,
            ..p.clone()
        }
        .check_upgrades()
        .is_err());
        const FEE_TOO_MUCH: &[Upgrade] = &[Upgrade {
            carrier_fee_bps: Some(10_001),
            ..Upgrade::new(3, 15)
        }];
        assert!(Params {
            upgrades: FEE_TOO_MUCH,
            ..p.clone()
        }
        .check_upgrades()
        .is_err());
        // Upgrade table: from version 3 (activation point 15) the carrier gets 10% and 30% of the fees
        const UPGRADES: &[Upgrade] = &[Upgrade {
            carrier_bps: Some(1_000),
            carrier_fee_bps: Some(3_000),
            ..Upgrade::new(3, 15)
        }];
        let q = Params {
            upgrades: UPGRADES,
            ..p
        };
        assert_eq!(q.check_upgrades(), Ok(()));
        assert_eq!(q.block_carrier_at(14), 25, "still 5% before activation");
        assert_eq!(q.block_carrier_at(15), 50, "10% after activation");
        assert_eq!(q.carrier_fee_at(14, 1_000), 500);
        assert_eq!(q.carrier_fee_at(15, 1_000), 300);
        assert_eq!(DEVNET.carrier_bps, 500);
        assert_eq!(DEVNET.carrier_fee_bps, 5_000);
        assert_eq!(DEVNET.carrier_repost_delay, 2);
    }

    /// D-076: the upgrade table layers the rules by DAA score; before and after the activation point and across
    /// several upgrades, the rules not changed stay the same.
    #[test]
    fn rules_follow_the_upgrade_schedule() {
        const UPGRADES: &[Upgrade] = &[
            Upgrade {
                block_gas_limit: Some(200_000_000),
                max_vmop_version: Some(3),
                ..Upgrade::new(3, 100)
            },
            Upgrade {
                native_max_gas: Some(2_000_000),
                min_vmop_version: Some(3),
                ..Upgrade::new(4, 250)
            },
        ];
        let p = Params {
            upgrades: UPGRADES,
            ..DEVNET
        };
        assert_eq!(p.check_upgrades(), Ok(()));
        let base = Rules {
            version: 2,
            block_gas_limit: 100_000_000,
            native_max_gas: 1_000_000,
            min_vmop_version: 2,
            max_vmop_version: 2,
            block_share_bps: 2_000,
            block_share_end: 50_457_600,
            carrier_bps: 500,
            carrier_fee_bps: 5_000,
        };
        assert_eq!(DEVNET.rules(0), base);
        assert_eq!(DEVNET.rules(u64::MAX), base);
        assert_eq!(p.rules(0), base);
        assert_eq!(p.rules(99), base);
        let v3 = Rules {
            version: 3,
            block_gas_limit: 200_000_000,
            max_vmop_version: 3,
            ..base
        };
        assert_eq!(p.rules(100), v3);
        assert_eq!(p.rules(249), v3);
        let v4 = Rules {
            version: 4,
            native_max_gas: 2_000_000,
            min_vmop_version: 3,
            ..v3
        };
        assert_eq!(p.rules(250), v4);
        assert_eq!(p.rules(u64::MAX), v4);
        assert!(v3.accepts_vmop_version(2) && v3.accepts_vmop_version(3));
        assert!(!v3.accepts_vmop_version(4) && !v4.accepts_vmop_version(2));
        assert_eq!((p.protocol_version(99), p.protocol_version(100)), (2, 3));
        assert_eq!(p.next_upgrade(0).map(|u| u.version), Some(3));
        assert_eq!(p.next_upgrade(100).map(|u| u.version), Some(4));
        assert_eq!(p.next_upgrade(250), None);
        assert_eq!((p.max_known_version(), DEVNET.max_known_version()), (4, 2));
    }

    #[test]
    fn bad_upgrade_schedules_are_rejected() {
        const BAD: [(&str, &[Upgrade]); 7] = [
            (
                "version not greater than the base version",
                &[Upgrade::new(2, 10)],
            ),
            ("activation point is 0", &[Upgrade::new(3, 0)]),
            (
                "version not increasing",
                &[Upgrade::new(3, 10), Upgrade::new(3, 20)],
            ),
            (
                "activation point not increasing",
                &[Upgrade::new(3, 10), Upgrade::new(4, 10)],
            ),
            (
                "gas limit is 0",
                &[Upgrade {
                    block_gas_limit: Some(0),
                    ..Upgrade::new(3, 10)
                }],
            ),
            (
                "native op limit exceeds the block limit",
                &[Upgrade {
                    native_max_gas: Some(DEVNET.block_gas_limit + 1),
                    ..Upgrade::new(3, 10)
                }],
            ),
            (
                "VmOp version range is empty",
                &[Upgrade {
                    min_vmop_version: Some(3),
                    ..Upgrade::new(3, 10)
                }],
            ),
        ];
        for (name, upgrades) in BAD {
            let p = Params { upgrades, ..DEVNET };
            assert!(p.check_upgrades().is_err(), "{name}");
        }
        assert_eq!(DEVNET.check_upgrades(), Ok(()));
        assert_eq!(SIGNET.check_upgrades(), Ok(()));
        assert!(DEVNET.upgrades.is_empty() && SIGNET.upgrades.is_empty());
    }
}
