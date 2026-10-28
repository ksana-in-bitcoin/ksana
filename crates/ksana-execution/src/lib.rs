//! Ksana block execution: entry processing (path B / C and unlock entries; path A and arrears have been removed,
//! D-078), the built-in Account / Asset modules, fee and nonce rules, locking and unlocking of sponsored fees, payout
//! of block rewards (at unlock time, D-079), genesis state (SPEC §4–§10; node and indexer share the same execution
//! code).
//!
//! - [`Executor`]: executes (validates) a block, or the producer builds a block entry by entry via [`BlockBuilder`];
//!   the rules for each kind of entry are in the module docs of `src/executor.rs`.
//! - [`BtcView`]: the BTC data at the anchor that execution needs (block headers, whether a block has entered the
//!   settled closure, native transactions, D-035, D-038, D-078).
//! - [`BlockEnv`]: the block context, including the unlockable segment computed by the node and indexer from the DAG
//!   ([`UnlockItem`], D-079).
//!   The execution layer only performs state-related checks; the settlement transaction itself is validated in
//!   `ksana-btc`.
//! - Results come in three kinds: failed op (receipt `Failed`), invalid block ([`BlockError::Invalid`]), local fault
//!   ([`BlockError::Fatal`], including storage and BTC data source failures, which must never be used to judge the
//!   block invalid).
//! - [`keys`]: encoding of state keys and values (`docs/ABI.md` section 7).
//!
//! This crate is consensus code: it must be deterministic, `unwrap` / `panic` are forbidden, and errors are always
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

pub mod builtin;
mod executor;
mod genesis;
pub mod genesis_file;
mod host;
pub mod keys;
mod state;

pub use executor::{
    check_entries_static, check_native_op, BlockBuilder, BlockEnv, BlockError, BlockOutcome,
    BtcView, Executor, MergedBlock, NoBtc, OpStatus, Receipt, UnlockItem,
};
pub use genesis::GenesisConfig;
pub use genesis_file::{Allocation, GenesisError, GenesisFile, BLOCK_SHARE_ACCOUNT_PLACEHOLDER};
pub use state::{AssetChange, ChangeKind, Event, MemState, Overlay, StateSource, WriteSet};
