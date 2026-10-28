//! Ksana standalone indexer (SPEC §10.3, §10.4; D-038, D-043, D-046, D-048, D-049, D-063).
//!
//! The indexer is the source of final truth: it connects only to bitcoind, never to any fast-lane node, reads the same
//! genesis config as the nodes, rebuilds the fast-lane block DAG and all state from BTC data alone, and replays block
//! by block with the same execution engine as the nodes (`ksana-execution`), verifying the `state_root` in the block
//! headers. Its answers must be exactly the same as the nodes', so the BTC semantics always use the implementation
//! shared with the nodes rather than a separate copy:
//!
//! - **BTC scanning**: the same tracker as the nodes (`ksana_btc::tracker::BtcTracker`, D-049): native transactions
//!   (§7.4), block headers and bodies in settlement and republish payloads (the transactions that land each fast-lane
//!   block's data on BTC, D-077); on a BTC reorg it truncates to the fork point and rescans. The indexer implements
//!   [`KnownBlocks`](ksana_btc::tracker::KnownBlocks): it collects block headers and candidate bodies from the scanned
//!   payloads (`observe_payload`) and answers body information (`block_data`), so whether a payload carried a block's
//!   body is decided by the tracker itself, the same as on the nodes.
//! - **Block DAG** (the [`ksana_btc::headers::HeaderDag`] shared with the nodes): block headers are admitted under the
//!   nodes' header rules (GHOSTDAG parent and merge limits, version, height, `bits` by `daa_score`, PoW, anchor height
//!   not below the parents'), and those with unknown parents are held back; the main chain is the same as the nodes':
//!   the selected chain of the tip with the largest fork-choice key `(chain_work, hash)` (blue work plus the block's
//!   own work, D-100) that can land on BTC.
//! - **Block bodies** ([`known`]): the entries of a block without a settlement transaction (D-043) are exactly the
//!   forced-inclusion entries derived per §7.4 (the window starts at the highest anchor among the parents), verified
//!   against `entries_root`; the entries of other blocks are taken from the payload claiming to settle them, likewise
//!   verified against `entries_root`.
//! - **Processing**: only the prefix of the main chain whose blocks and merged blocks all have their data landed on BTC
//!   (the settled closure, D-038, D-063) is processed; block by block it checks the body rules of the block and of the
//!   blocks it merges (state-independent entry rules, forced inclusion, D-036) and the settlement backlog limit on
//!   main-chain blocks (D-046), executes the merged blocks in lenient mode first and then the block itself in strict
//!   mode, verifies the `state_root`, and commits as state version = height. When a main-chain block's `state_root`
//!   does not match, strict execution fails or the backlog limit is violated, that block cannot land: it is marked, the
//!   main chain is re-selected and processing continues (same as the nodes, D-063); a block with an invalid body, or one
//!   merging such a block, is marked in the same way, because the nodes reject it and every block after it (§10.3 item
//!   5, D-098). Processing stops with an alarm only when such a block is already BTC-final (D-082). bitcoind and
//!   storage failures are local faults: retried in the next round and never grounds for judging a block invalid.
//! - **Rollback**: when a BTC reorg or a main-chain switch takes executed blocks out of the main chain or the prefix,
//!   the state is rolled back to the highest height still valid (`ksana-state`); beyond `REORG_WINDOW` (old state
//!   already pruned) it is rebuilt from genesis. After a restart the BTC snapshot is rescanned and the latest version
//!   of the state store is verified against the main chain by state root; while the processable prefix is still below
//!   the oldest version kept (bitcoind is behind), the verification waits instead of rebuilding (D-098).
//! - **Output**: [`Indexer::status`], [`Indexer::block_at`] and JSON-RPC ([`rpc`]: `indexer_status`,
//!   `indexer_stateRoot`).
//!
//! The indexer is not consensus code, but everything it processes is untrusted BTC data: panics are likewise forbidden;
//! it does not read the system time.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]

mod index;
mod indexer;
pub mod known;
mod meta;
pub mod rpc;

pub use indexer::{Indexer, IndexerConfig, IndexerStatus, Progress};

/// Indexer errors. All except [`IndexerError::Config`] are local faults: just retry later; they say nothing about any
/// fast-lane block being invalid (an invalid block is not an error but a halt, see [`IndexerStatus::halted`]).
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// The config or data directory is incorrect (genesis config, data directory belongs to a different config, the
    /// genesis anchor block is not on bitcoind's best chain).
    #[error("config error: {0}")]
    Config(String),
    /// Local storage error.
    #[error("storage error: {0}")]
    Storage(String),
    /// bitcoind failure or not yet synced.
    #[error("BTC data source error: {0}")]
    Btc(String),
    /// Local fault while executing a block (storage, VM engine, BTC data source).
    #[error("local fault while executing a block: {0}")]
    Execution(String),
    /// Other internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<ksana_state::StateError> for IndexerError {
    fn from(e: ksana_state::StateError) -> Self {
        IndexerError::Storage(e.to_string())
    }
}

impl From<ksana_execution::GenesisError> for IndexerError {
    fn from(e: ksana_execution::GenesisError) -> Self {
        match e {
            ksana_execution::GenesisError::Config(m) => IndexerError::Config(m),
        }
    }
}

impl From<ksana_btc::tracker::TrackerError> for IndexerError {
    fn from(e: ksana_btc::tracker::TrackerError) -> Self {
        use ksana_btc::tracker::TrackerError;
        match e {
            TrackerError::Btc(m) | TrackerError::TooDeep(m) => IndexerError::Btc(m),
            TrackerError::Config(m) => IndexerError::Config(m),
            TrackerError::Known(m) | TrackerError::Internal(m) => IndexerError::Internal(m),
        }
    }
}
