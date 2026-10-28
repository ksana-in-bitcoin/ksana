//! Ksana consensus (SPEC §6): the replaceable [`Consensus`] trait and the PoW implementation (mining algorithm Argon2id,
//! D-094).
//!
//! This crate is consensus code: it must be deterministic, `unwrap` / `panic` are forbidden, errors are always returned.
//!
//! - [`U256`]: the 256-bit unsigned integer used for targets, PoW values and work.
//! - [`target_from_bits`] / [`bits_from_target`] / [`work_from_bits`] / [`pow_value`]:
//!   Bitcoin's compact encoding, work and hash byte-order conventions (D-025).
//! - [`effective_target`]: two-speed difficulty (D-084, D-090): the PoW target of an empty block is the `bits` target
//!   divided by its weight (the DAA score increment, the number of reward slots); the weight doubles per block from N
//!   up to the cap according to the number of consecutive empty blocks on the selected chain.
//! - [`asert_target`] / [`Asert`]: integer ASERT anchored at the genesis block (aserti3-2d, D-026).
//! - [`median_time_past`] / [`check_timestamp`]: the timestamp rules when receiving a new block (SPEC §6.3).
//! - [`genesis_header`]: constructs the genesis block header per D-027.
//! - [`PowAlgorithm`] / [`Argon2id`] / [`pow_algorithm`]: the mining algorithm decoupled from consensus, selected by the
//!   network params (D-062); the network's algorithm is Argon2id (D-094), [`Sha256d`] is only for tests and benchmarks.
//! - [`BlockDag`] / [`GhostdagData`]: the block DAG and GHOSTDAG (selected parent, merge set, coloring, blue score and
//!   blue work, D-063).
//! - [`Consensus`] / [`PowConsensus`]: block header checks, the next block's `bits`, work and PoW solving.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::float_arithmetic,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::iter_over_hash_type
)]
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]

mod algorithm;
mod asert;
mod genesis;
mod ghostdag;
mod pow;
mod pow_consensus;
mod time;
mod u256;

pub use algorithm::{pow_algorithm, Argon2id, PowAlgorithm, Sha256d};
pub use asert::{asert_target, Asert};
pub use genesis::{empty_entries_root, genesis_header};
pub use ghostdag::{
    BlockDag, DagBlock, DagCheckpoint, GhostdagData, GhostdagError, GhostdagParams, Hash,
    MergeRootCache, StoredBlock,
};
pub use pow::{
    bits_from_target, check_pow, effective_target, pow_limit_target, pow_value, target_from_bits,
    work_from_bits, work_from_target,
};
pub use pow_consensus::PowConsensus;
pub use time::{check_anchor_time, check_timestamp, median_time_past};
pub use u256::U256;

use ksana_primitives::BlockHeader;

/// Consensus error. A header check returning an error means that header is invalid, except for:
/// [`ConsensusError::InvalidPowLimit`] and [`ConsensusError::InvalidParams`], which mean a local parameter
/// configuration error, and [`ConsensusError::PowUnavailable`], which means a local computation failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConsensusError {
    /// `bits` is not a usable target: sign bit set, target 0, target ≥ 2^256, or target above the PoW limit (D-025).
    #[error(
        "bits {0:#010x} invalid (sign bit set, target 0, target ≥ 2^256 or above the PoW limit)"
    )]
    InvalidBits(u32),
    /// The PoW limit itself is not a valid compact target (parameter configuration error).
    #[error("PoW limit {0:#010x} is not a valid compact target")]
    InvalidPowLimit(u32),
    /// Another protocol parameter is unusable, e.g. an ASERT half-life of 0.
    #[error("invalid protocol params: {0}")]
    InvalidParams(&'static str),
    /// The block's `bits` is not bit-for-bit identical to the value computed by the difficulty adjustment (D-025).
    #[error(
        "bits does not match the difficulty adjustment: expected {expected:#010x}, got {got:#010x}"
    )]
    BitsMismatch {
        /// The `bits` computed by the difficulty adjustment.
        expected: u32,
        /// The `bits` in the block header.
        got: u32,
    },
    /// The PoW value ([`PowAlgorithm::pow_value`]) is greater than the effective target (the target represented by
    /// `bits` divided by the weight, D-084, D-090).
    #[error("block header does not meet the PoW target")]
    InsufficientPow,
    /// The PoW value cannot be computed locally (e.g. allocating Argon2id's memory failed, D-094): a local fault, it
    /// does not mean the block is invalid, and the caller must not reject the block because of it.
    #[error("cannot compute the PoW value (local fault): {0}")]
    PowUnavailable(String),
    /// The target of `bits` divided by the weight is 0 (target too small to be met), or the weight is 0 (D-084): the
    /// block header is invalid.
    #[error(
        "target of bits {bits:#010x} divided by weight {weight} is 0 and cannot be met (D-084)"
    )]
    TargetTooSmall {
        /// The `bits` in the block header.
        bits: u32,
        /// The weight of the block header.
        weight: u64,
    },
    /// The block header version is higher than the highest version in this software's upgrade table (D-076): an
    /// upgrade this software does not know, so whether the block is valid cannot be decided.
    #[error("block header version {got} is higher than the highest version {max_known} supported by this software: a software upgrade is required (D-076)")]
    UnknownVersion {
        /// The version in the block header.
        got: u32,
        /// [`ksana_primitives::Params::max_known_version`].
        max_known: u32,
    },
    /// The block header version is not the protocol version applicable at the block's DAA score (D-027, D-076).
    #[error("block header version should be {expected}, got {got}")]
    BadVersion {
        /// The protocol version at that DAA score ([`ksana_primitives::Params::protocol_version`]).
        expected: u32,
        /// The version in the block header.
        got: u32,
    },
    /// The block height is not the parent height plus 1 (or the genesis block height is not 0).
    #[error("block height should be {expected}, got {got}")]
    BadHeight {
        /// The expected height.
        expected: u64,
        /// The height in the block header.
        got: u64,
    },
    /// `parents[0]` is not the selected parent, or there are no parents.
    #[error("parents[0] is not the selected parent")]
    BadParent,
    /// The parents other than the selected parent are not in strictly ascending byte order, or contain the selected
    /// parent.
    #[error("parents other than the selected parent must be in strictly ascending byte order and must not contain the selected parent")]
    BadParentOrder,
    /// The timestamp is not greater than the median of the previous blocks' timestamps (SPEC §6.3, only checked when
    /// receiving a new block).
    #[error("timestamp {got} is not greater than the median time {median} of the previous blocks")]
    TimestampNotAfterMedian {
        /// The median of the previous blocks' timestamps.
        median: u64,
        /// The timestamp in the block header.
        got: u64,
    },
    /// The timestamp exceeds the local time plus `max_future_block_time_secs` (SPEC §6.3, only checked when receiving
    /// a new block). Unlike the other errors, it does not mean the block is permanently invalid: once the local time
    /// catches up the same block may pass the check.
    #[error("timestamp {got} exceeds the allowed maximum {max}")]
    TimestampTooFarInFuture {
        /// The maximum allowed timestamp.
        max: u64,
        /// The timestamp in the block header.
        got: u64,
    },
    /// No ancestor timestamps were provided, so the median time cannot be computed.
    #[error("no ancestor timestamps available to compute the median time")]
    NoRecentTimestamps,
    /// The timestamp exceeds the time of the anchored BTC block header plus `max_timestamp_ahead_of_anchor_secs`
    /// (D-083, consensus rule).
    #[error(
        "timestamp {got} is too far ahead of the anchored BTC block's time (maximum {max}, D-083)"
    )]
    TimestampAheadOfAnchor {
        /// The maximum allowed timestamp.
        max: u64,
        /// The timestamp in the block header.
        got: u64,
    },
    /// Integer overflow (e.g. the parent height is already `u64::MAX`).
    #[error("integer overflow")]
    Overflow,
}

/// Replaceable consensus (SPEC §6). The mining algorithm is abstracted separately by [`PowAlgorithm`] (D-062); the
/// structural rules of the DAG are checked by [`BlockDag`] (GHOSTDAG, D-063).
///
/// Implementations must be deterministic and depend only on the block headers in the parameters (the time is passed
/// in by the caller, see [`check_timestamp`]).
pub trait Consensus: Send + Sync {
    /// The empty-block streak of a block header (D-090): the number of consecutive empty blocks on the selected chain
    /// up to and including it — 0 for a non-empty block (`entries_root` is not the root of the empty entry list), the
    /// selected parent's streak `selected_parent_streak` plus one for an empty block; the genesis block counts as 0.
    /// The selected parent's streak is given by [`BlockDag::parent_streak`].
    fn streak(&self, header: &BlockHeader, selected_parent_streak: u64) -> u64;

    /// The weight of a block header (two-speed difficulty, D-084; increasing with the streak, D-090): 1 for a non-empty
    /// block, `min(EMPTY_BLOCK_DIFFICULTY × 2^(s − 1), MAX_EMPTY_BLOCK_WEIGHT)` for the s-th empty block of a streak.
    /// It is the DAA score increment ([`BlockDag::compute`]), the divisor of the PoW effective target
    /// ([`effective_target`]) and the number of block reward slots.
    fn weight(&self, header: &BlockHeader, selected_parent_streak: u64) -> u64;

    /// The `bits` a block with selected parent `selected_parent`, DAA score `daa_score` and weight `weight` must carry:
    /// ASERT uses `daa_score − weight` (the selected parent's DAA score plus the weights of the blocks in the merge
    /// set, D-063, D-084) as the parent height and the selected parent's timestamp as the parent time.
    /// An empty block and a non-empty block with the same parents have the same `bits` (on a pure chain of non-empty
    /// blocks this equals the computation by parent height).
    fn next_bits(
        &self,
        selected_parent: &BlockHeader,
        daa_score: u64,
        weight: u64,
    ) -> Result<u32, ConsensusError>;

    /// Checks a non-genesis block header against its selected parent (D-063): `version` equals the protocol version at
    /// `daa_score` (D-076, `BLOCK_VERSION` when there is no upgrade); `parents[0] == selected_parent.hash()`,
    /// the remaining parents are in strictly ascending byte order and none equals `parents[0]`;
    /// `height == selected_parent.height + 1`; `bits == next_bits(selected_parent, daa_score, weight)`; the PoW meets
    /// the effective target (the target of `bits` divided by `weight`, D-084, D-090).
    /// Returns [`ConsensusError::PowUnavailable`] when the algorithm's computation fails (a local fault, not an invalid
    /// block).
    ///
    /// `weight` is this block's weight ([`Consensus::weight`], by the selected parent's streak), and `daa_score`
    /// includes it. The selected parent and `daa_score` must be computed by the caller per GHOSTDAG
    /// ([`BlockDag::parent_streak`], [`BlockDag::compute`]; the latter also checks the number of parents, that no
    /// parent is an ancestor of another, and the merge limit).
    /// `btc_anchor` and the timestamp rules are not checked here (they need the BTC chain and the local time, see
    /// [`check_timestamp`]).
    ///
    /// The same checks as [`Consensus::check_header_rules`] followed by [`Consensus::check_header_pow`].
    fn check_header(
        &self,
        selected_parent: &BlockHeader,
        daa_score: u64,
        weight: u64,
        header: &BlockHeader,
    ) -> Result<(), ConsensusError> {
        self.check_header_rules(selected_parent, daa_score, weight, header)?;
        self.check_header_pow(header, weight)
    }

    /// The part of [`Consensus::check_header`] that needs neither the selected parent nor the DAA score: the version is
    /// not above the highest this node knows ([`ConsensusError::UnknownVersion`], so that a block of an unknown upgrade
    /// is not judged by rules it may have changed, D-076), and the PoW meets the effective target of the header's own
    /// `bits` by `weight`. Block validation checks it before GHOSTDAG ([`BlockDag::compute`]), whose cost grows with
    /// the merged chains, so that a header without valid PoW does not cost that computation (local policy: which
    /// blocks are valid does not change, second audit P1-1).
    fn check_header_pow(&self, header: &BlockHeader, weight: u64) -> Result<(), ConsensusError>;

    /// [`Consensus::check_header`] without its PoW check, for a header that passed [`Consensus::check_header_pow`] with
    /// the same `weight`: since `bits` must equal [`Consensus::next_bits`], the PoW then meets that target too.
    fn check_header_rules(
        &self,
        selected_parent: &BlockHeader,
        daa_score: u64,
        weight: u64,
        header: &BlockHeader,
    ) -> Result<(), ConsensusError>;

    /// The work contributed by this block header with weight `weight` (fork choice is by cumulative work, SPEC §6.4):
    /// the work of the effective target ⌊2^256 / (effective target + 1)⌋ (D-025, D-084); an empty block is about
    /// `weight` times a non-empty block with the same `bits`.
    fn work(&self, header: &BlockHeader, weight: u64) -> Result<U256, ConsensusError>;

    /// The work of the genesis block: computed from the effective target of its own weight (it is an empty block,
    /// streak 1, D-084). It is never in any merge set, so this value does not affect consensus.
    fn genesis_work(&self, genesis: &BlockHeader) -> Result<U256, ConsensusError> {
        self.work(genesis, self.weight(genesis, 0))
    }

    /// The suggested number of nonces to try in one [`Consensus::solve`] call ([`PowAlgorithm::solve_chunk`]: about
    /// 10-20 milliseconds of work).
    fn solve_chunk(&self) -> u64;

    /// Tries the nonces in `[start, start + iterations)` in order (no wrap-around, up to `u64::MAX`).
    /// When a nonce meeting the effective target of `header.bits` (by weight `weight`, D-084, D-090) is found, writes
    /// it to `header.nonce` and returns `Ok(true)`; otherwise returns `Ok(false)` and `header` is unchanged. Returns
    /// `Ok(false)` right away when `bits` is invalid or the effective target is 0; returns
    /// [`ConsensusError::PowUnavailable`] when the algorithm's computation fails.
    fn solve(
        &self,
        header: &mut BlockHeader,
        weight: u64,
        start: u64,
        iterations: u64,
    ) -> Result<bool, ConsensusError>;
}
