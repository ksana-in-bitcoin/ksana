//! Ksana consensus primitives: types, Borsh encoding, hashing, protocol parameters (SPEC §4, §6.1, §7.1, §13).
//!
//! This crate is consensus code: it must be deterministic, `unwrap` / `panic` are forbidden, errors are always
//! returned.

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

pub mod asset;
pub mod codec;
pub mod compact;
pub mod compact_entries;
pub mod hash;
pub mod json;
pub mod params;
#[cfg(any(test, feature = "proptest"))]
pub mod strategies;
mod types;

pub use codec::CodecError;
pub use params::{
    Argon2Params, Network, Params, PowAlgorithmId, RewardSplit, Rules, Upgrade, DEVNET, MAINNET,
    SIGNET,
};
pub use types::{
    check_bodies, check_funds, Action, Block, BlockHeader, Body, BtcAnchor, Entry, EntryData,
    FeeMode, Funds, OutPoint, Payload, SettlementTxs, SigScheme, VmOp,
};

/// The current VmOp version (SPEC §4.1): from 2 on, `Call` / `Deploy` carry `funds` (D-073).
pub const VMOP_VERSION: u8 = 2;

/// The maximum number of asset kinds attached to one call (D-073).
pub const MAX_CALL_FUNDS: usize = 16;

/// The current block header version (the spec gives no value, D-027): 2 is the DAG header (D-063).
pub const BLOCK_VERSION: u32 = 2;

/// The asset ID of the project token KSA: 32 zero bytes (SPEC §4.4, §8.6).
pub const KSA_ASSET_ID: [u8; 32] = [0u8; 32];

#[cfg(test)]
mod tests;
