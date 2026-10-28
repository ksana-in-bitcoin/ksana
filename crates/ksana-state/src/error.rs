//! Error type of the state store.

use crate::Version;

/// State store error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    /// RocksDB returned an error.
    #[error("RocksDB error: {0}")]
    Db(#[from] rocksdb::Error),
    /// Jellyfish Merkle Tree computation failed (usually means the storage is corrupted).
    #[error("JMT error: {0}")]
    Jmt(String),
    /// Stored data does not have the expected format or is missing (database corrupted, or not a database written by
    /// this crate).
    #[error("state database corrupted: {0}")]
    Corrupted(String),
    /// The database has no committed version yet.
    #[error("state database is empty, no committed version")]
    Empty,
    /// The version passed to `commit` is not `latest + 1`.
    #[error("cannot commit version {got}: the next version should be {expected}")]
    UnexpectedVersion {
        /// The version that should be committed (`latest + 1`).
        expected: Version,
        /// The version actually given.
        got: Version,
    },
    /// The version is outside the available range `[oldest, latest]` (pruned, rolled back or not yet committed).
    #[error("version {version} is unavailable (available range {oldest}..={latest})")]
    VersionUnavailable {
        /// The requested version.
        version: Version,
        /// The oldest available version.
        oldest: Version,
        /// The latest version.
        latest: Version,
    },
    /// Version number out of range: `u64::MAX` is reserved by jmt (`PRE_GENESIS_VERSION`) and cannot be committed.
    #[error("version number {0} is out of range")]
    VersionOutOfRange(Version),
    /// The view's version has been rolled back or pruned (or re-committed with different content after a rollback).
    #[error(
        "state view of version {version} is stale: that version has been rolled back or pruned"
    )]
    StaleView {
        /// The view's version.
        version: Version,
    },
}
