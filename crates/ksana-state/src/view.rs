//! [`StateView`]: a read-only view pinned to a version.

use std::fmt;
use std::sync::Arc;

use rocksdb::DB;

use crate::store::{read_root, read_value_at};
use crate::{key_hash, StateError, StateRead, Version};

/// Read-only state view pinned to a version: cheap, owned, `'static`, thread-safe (`Send + Sync`), can be put into a
/// wasmtime `Store`.
///
/// Holds an `Arc` of the underlying RocksDB handle, the version number and that version's root at creation time. New
/// versions committed later by [`StateDb`](crate::StateDb) do not affect what the view reads. Every read first checks,
/// within the same RocksDB snapshot, that "the root of that version is still the root at creation time" before
/// reading the value: if the version was later removed by `rollback_to` or `prune` (or re-committed with different
/// content after a rollback), the read returns [`StateError::StaleView`] and never reads data of another branch.
/// Views do not prevent rollback or pruning.
///
/// Views share the same RocksDB instance as the `StateDb`: the database closes only after the `StateDb` and all the
/// views it created are released; reopening the same path before that returns an error (RocksDB's file lock).
#[derive(Clone)]
pub struct StateView {
    db: Arc<DB>,
    version: Version,
    root: [u8; 32],
}

impl StateView {
    pub(crate) fn new(db: Arc<DB>, version: Version, root: [u8; 32]) -> Self {
        Self { db, version, root }
    }

    /// The view's version.
    pub fn version(&self) -> Version {
        self.version
    }
}

impl StateRead for StateView {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StateError> {
        let snapshot = self.db.snapshot();
        if read_root(&self.db, Some(&snapshot), self.version)? != Some(self.root) {
            return Err(StateError::StaleView {
                version: self.version,
            });
        }
        read_value_at(&self.db, Some(&snapshot), &key_hash(key), self.version)
    }
}

impl fmt::Debug for StateView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let root: String = self.root.iter().map(|byte| format!("{byte:02x}")).collect();
        f.debug_struct("StateView")
            .field("version", &self.version)
            .field("root", &root)
            .finish()
    }
}
