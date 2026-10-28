//! Ksana fast-lane node (SPEC §6, §7, §10.2, §11; M2 single-node block production and path B, M3 BTC anchoring and
//! settlement, M4 multi-node network, fork choice and reorg).
//!
//! - [`chain::Chain`]: state (`ksana-state`) + block store ([`store::ChainStore`], keeps every block whose header is
//!   valid) + consensus (`ksana-consensus`) + executor; block tree, fork choice and reorg (D-041), recovers from the
//!   store at startup and checks the state root.
//! - [`btc`]: the BTC snapshot synced with bitcoind and the execution layer's BTC view (D-035); [`settlement`]:
//!   settlement payloads and settlement transactions (D-034, D-037).
//! - [`producer`]: the block production loop: forced inclusion, unlock, candidates from the entry pool, building the
//!   block entry by entry, solving the PoW, signing the settlement transactions, submitting.
//! - [`ops`]: op admission and entry pool maintenance after main-chain changes (shared by RPC and the network);
//!   [`rpc`]: the backend implementation of `ksana-rpc`; [`standards`]: the index of the token standards' events
//!   (D-120) the block store keeps beside the operation index.
//! - [`rescue`]: rescue settlement and header republishing (D-045, D-047).
//! - [`net`]: integration with the network layer (`ksana-p2p`): received blocks and ops go to the chain and the entry
//!   pool, this node's blocks and ops are broadcast to the network.
//! - [`node::Node`]: assembles all of the above for the binary and the tests.
//!
//! The node is not consensus code, but everything it handles is untrusted input: panics are forbidden all the same.
//! The system time is read in one place only, [`unix_now`].

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

pub mod btc;
pub mod chain;
pub mod genesis;
pub mod net;
pub mod node;
pub mod ops;
pub mod producer;
pub mod rescue;
pub mod rpc;
pub mod settlement;
pub mod standards;
pub mod store;

/// Node error.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The config or the data directory is incorrect.
    #[error("config error: {0}")]
    Config(String),
    /// Local storage error.
    #[error("storage error: {0}")]
    Storage(String),
    /// The block is invalid (consensus rules).
    #[error("invalid block: {0}")]
    InvalidBlock(String),
    /// The block does not satisfy the acceptance rules, or disagrees with the local BTC view (the anchor is not on
    /// the local BTC best chain, the anchor lags, the timestamp, the inputs of the settlement transactions are
    /// currently spent): this copy is rejected, but that does not say the block is invalid (D-041).
    #[error("block not accepted for now: {0}")]
    Rejected(String),
    /// The main-chain tip changed between validation and commit (for example a block from another node arrived):
    /// the caller discards and retries.
    #[error("main-chain tip has changed")]
    TipChanged,
    /// The local BTC data source (bitcoind) failed or is not yet synced: the block must not be judged invalid on
    /// that basis.
    #[error("BTC data source error: {0}")]
    Btc(String),
    /// Other internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<ksana_state::StateError> for NodeError {
    fn from(e: ksana_state::StateError) -> Self {
        NodeError::Storage(e.to_string())
    }
}

impl From<rocksdb::Error> for NodeError {
    fn from(e: rocksdb::Error) -> Self {
        NodeError::Storage(e.to_string())
    }
}

impl From<ksana_consensus::ConsensusError> for NodeError {
    fn from(e: ksana_consensus::ConsensusError) -> Self {
        match e {
            // An upgrade this node does not know (D-076): cannot decide whether the block is valid, reject it without
            // judging it invalid
            ksana_consensus::ConsensusError::UnknownVersion { .. } => {
                NodeError::Rejected(e.to_string())
            }
            // The PoW value cannot be computed locally (D-094) or the params are misconfigured: local fault, the
            // block is not judged invalid
            ksana_consensus::ConsensusError::PowUnavailable(_)
            | ksana_consensus::ConsensusError::InvalidPowLimit(_)
            | ksana_consensus::ConsensusError::InvalidParams(_) => {
                NodeError::Internal(e.to_string())
            }
            other => NodeError::InvalidBlock(other.to_string()),
        }
    }
}

impl From<ksana_execution::BlockError> for NodeError {
    fn from(e: ksana_execution::BlockError) -> Self {
        match e {
            ksana_execution::BlockError::Invalid(m) => NodeError::InvalidBlock(m),
            ksana_execution::BlockError::Fatal(m) => NodeError::Internal(m),
        }
    }
}

impl From<ksana_primitives::CodecError> for NodeError {
    fn from(e: ksana_primitives::CodecError) -> Self {
        NodeError::Internal(e.to_string())
    }
}

impl From<std::io::Error> for NodeError {
    fn from(e: std::io::Error) -> Self {
        NodeError::Storage(e.to_string())
    }
}

/// The current Unix time (seconds). The only place in the node that reads the system time: used for the timestamp of
/// new blocks and for the time rules when receiving new blocks (SPEC §6.3), never enters the execution layer.
#[allow(clippy::disallowed_methods)]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Monotonic clock (used by the block production loop to decide whether the template is stale and by the chain to
/// age the refusals it remembers, never enters block contents).
#[allow(clippy::disallowed_methods)]
pub fn monotonic_now() -> std::time::Instant {
    std::time::Instant::now()
}
