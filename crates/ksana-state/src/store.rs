//! [`StateDb`]: versioned JMT state on RocksDB.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use jmt::storage::TreeUpdateBatch;
use jmt::KeyHash;
use rocksdb::{
    ColumnFamily, PrefixRange, ReadOptions, SnapshotWithThreadMode, WriteBatch, WriteOptions, DB,
};

use crate::rocks::{self, DbMemory};
use crate::schema::{
    change_key, decode_stored_value, encode_stored_value, root_node_key, split_version, stale_key,
    value_key, Meta, CF_CHANGES, CF_LATEST, CF_META, CF_NODES, CF_ROOTS, CF_STALE, CF_VALUES,
    COLUMN_FAMILIES, META_KEY, VERSIONS_END,
};
use crate::{key_hash, schema, tree, StateError, StateRead, StateView, Version, WriteSet};

/// Versioned state database: Jellyfish Merkle Tree + RocksDB. See the crate documentation for the design.
///
/// Write operations ([`commit`](Self::commit), [`rollback_to`](Self::rollback_to), [`prune`](Self::prune)) need
/// `&mut self`, read operations only `&self`; only one `StateDb` can be open at the same path at a time (RocksDB's
/// file lock).
pub struct StateDb {
    db: Arc<DB>,
    /// Available version range; `None` means no version has been committed yet. Kept consistent with the metadata
    /// in the database.
    meta: Option<Meta>,
}

/// Result computed before a commit (not yet written to the database).
struct Prepared<'w> {
    root: [u8; 32],
    update: TreeUpdateBatch,
    /// Writes that really change the state (no-ops filtered out), sorted by full key.
    changes: Vec<(&'w [u8], Option<&'w [u8]>)>,
}

impl StateDb {
    /// Opens (creating it if it does not exist) the state database at `path`, with a block cache of its own.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        Self::open_with(path, &DbMemory::new())
    }

    /// Opens (creating it if it does not exist) the state database at `path`, within the RocksDB memory bounds of
    /// `memory`, whose block cache it shares with the process's other databases (D-103).
    pub fn open_with(path: &Path, memory: &DbMemory) -> Result<Self, StateError> {
        let db = memory.open(path, &COLUMN_FAMILIES)?;
        let meta = match db.get_pinned_cf(cf_handle(&db, CF_META)?, META_KEY)? {
            Some(bytes) => Some(Meta::decode(&bytes)?),
            None => None,
        };
        let state = Self {
            db: Arc::new(db),
            meta,
        };
        if let Some(meta) = state.meta {
            // Rough consistency check: the latest version must have a root.
            state.root(meta.latest)?;
        }
        Ok(state)
    }

    /// The bytes RocksDB's write buffers of this database hold (D-103).
    pub fn memtable_bytes(&self) -> Result<u64, StateError> {
        Ok(rocks::memtable_bytes(&self.db, &COLUMN_FAMILIES)?)
    }

    /// The latest committed version; `None` for an empty database.
    pub fn latest_version(&self) -> Option<Version> {
        self.meta.map(|meta| meta.latest)
    }

    /// The oldest version that can still be read and rolled back to; `None` for an empty database.
    pub fn oldest_version(&self) -> Option<Version> {
        self.meta.map(|meta| meta.oldest)
    }

    /// The state root of version `version`. Returns an error when the version is not within `[oldest, latest]`.
    pub fn root(&self, version: Version) -> Result<[u8; 32], StateError> {
        self.check_available(version)?;
        read_root(&self.db, None, version)?
            .ok_or_else(|| StateError::Corrupted(format!("missing root of version {version}")))
    }

    /// The value of `key` in the latest version; always `None` for an empty database.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StateError> {
        let cf = cf_handle(&self.db, CF_LATEST)?;
        Ok(self.db.get_pinned_cf(cf, key)?.map(|value| value.to_vec()))
    }

    /// The value of `key` in version `version`. Returns an error when the version is not within `[oldest, latest]`.
    pub fn get_at(&self, key: &[u8], version: Version) -> Result<Option<Vec<u8>>, StateError> {
        self.check_available(version)?;
        read_value_at(&self.db, None, &key_hash(key), version)
    }

    /// Commits `writes` as version `version` and returns the new state root.
    ///
    /// `version` must be `latest + 1`; for an empty database it can be any value (genesis can be at 0), except
    /// `u64::MAX`.
    /// Deleting a non-existent key and writing a value equal to the current one are ignored (the root does not change
    /// and no data is written); an empty write set still produces a new version.
    /// All data (including the version metadata) is written synchronously in one RocksDB `WriteBatch`.
    pub fn commit(&mut self, version: Version, writes: &WriteSet) -> Result<[u8; 32], StateError> {
        self.check_next_version(version)?;
        let Prepared {
            root,
            update,
            changes,
        } = self.prepare(version, writes)?;

        let nodes = cf_handle(&self.db, CF_NODES)?;
        let stale = cf_handle(&self.db, CF_STALE)?;
        let values = cf_handle(&self.db, CF_VALUES)?;
        let latest = cf_handle(&self.db, CF_LATEST)?;
        let change_log = cf_handle(&self.db, CF_CHANGES)?;
        let roots = cf_handle(&self.db, CF_ROOTS)?;
        let meta_cf = cf_handle(&self.db, CF_META)?;

        let mut batch = WriteBatch::default();
        for (node_key, node) in update.node_batch.nodes() {
            let bytes = borsh::to_vec(node)
                .map_err(|e| StateError::Jmt(format!("JMT node cannot be encoded: {e}")))?;
            batch.put_cf(nodes, schema::node_key(node_key)?, bytes);
        }
        for index in &update.stale_node_index_batch {
            let node_db_key = schema::node_key(&index.node_key)?;
            batch.put_cf(
                stale,
                stale_key(index.stale_since_version, &node_db_key),
                b"",
            );
        }
        if let Some(meta) = self.meta {
            // The previous version's root node is no longer reachable as of this version. jmt does not mark it stale
            // when "this version has no change (the old root is copied)" or "the old root is the empty tree", so it is
            // always recorded here (the same key jmt itself would record), otherwise pruning could not delete it.
            batch.put_cf(stale, stale_key(version, &root_node_key(meta.latest)), b"");
        }
        for &(key, value) in &changes {
            batch.put_cf(
                values,
                value_key(&key_hash(key), version),
                encode_stored_value(value),
            );
            match value {
                Some(value) => batch.put_cf(latest, key, value),
                None => batch.delete_cf(latest, key),
            }
            batch.put_cf(change_log, change_key(version, key), b"");
        }
        batch.put_cf(roots, version.to_be_bytes(), root);
        let meta = Meta {
            oldest: self.meta.map_or(version, |meta| meta.oldest),
            latest: version,
        };
        batch.put_cf(meta_cf, META_KEY, meta.encode());

        self.write(batch)?;
        self.meta = Some(meta);
        Ok(root)
    }

    /// The root that `commit(latest + 1, writes)` would produce, without writing any data. For an empty database it is
    /// the root obtained by creating genesis with `writes` (the root does not depend on the version number).
    pub fn preview_root(&self, writes: &WriteSet) -> Result<[u8; 32], StateError> {
        let version = match self.meta {
            None => 0,
            Some(meta) => meta
                .latest
                .checked_add(1)
                .ok_or(StateError::VersionOutOfRange(meta.latest))?,
        };
        self.check_next_version(version)?;
        Ok(self.prepare(version, writes)?.root)
    }

    /// The root that committing `writes` on top of version `base` would produce, without writing any data: the same
    /// root as `commit(base + 1, writes)` after `rollback_to(base)` (for judging a block whose parent is not the latest
    /// version, D-101). `base` must be within `[oldest, latest]`. Writes are checked for no-ops against `base`. The
    /// versions above `base` are left as they are; the caller keeps commits, rollbacks and pruning out meanwhile (they
    /// need `&mut self`).
    pub fn preview_root_at(
        &self,
        base: Version,
        writes: &WriteSet,
    ) -> Result<[u8; 32], StateError> {
        self.check_available(base)?;
        let version = base
            .checked_add(1)
            .filter(|v| *v != u64::MAX)
            .ok_or(StateError::VersionOutOfRange(base))?;
        let mut value_set = Vec::new();
        for (key, value) in writes {
            let hash = key_hash(key);
            let current = read_value_at(&self.db, None, &hash, base)?;
            if current.as_deref() == value.as_deref() {
                continue;
            }
            value_set.push((KeyHash(hash), value.clone()));
        }
        // jmt starts from the root at `version − 1 = base` and only reads nodes reachable from it (all at or below
        // `base`); the nodes it creates for `version` stay in memory, so the committed versions above `base` do not
        // interfere
        let (root, _) = tree::apply(&self.db, Some(base), version, value_set)?;
        Ok(root)
    }

    /// Rolls back to `version`: deletes everything committed after it, and `version` becomes the latest version again.
    /// Returns an error when `version` is not within `[oldest, latest]`; does nothing when `version == latest`.
    pub fn rollback_to(&mut self, version: Version) -> Result<(), StateError> {
        let meta = self.check_available(version)?;
        if version == meta.latest {
            return Ok(());
        }
        // version < latest < u64::MAX, cannot overflow.
        let first_removed = version
            .checked_add(1)
            .ok_or(StateError::VersionOutOfRange(version))?
            .to_be_bytes();

        let nodes = cf_handle(&self.db, CF_NODES)?;
        let stale = cf_handle(&self.db, CF_STALE)?;
        let values = cf_handle(&self.db, CF_VALUES)?;
        let latest = cf_handle(&self.db, CF_LATEST)?;
        let change_log = cf_handle(&self.db, CF_CHANGES)?;
        let roots = cf_handle(&self.db, CF_ROOTS)?;
        let meta_cf = cf_handle(&self.db, CF_META)?;

        let mut batch = WriteBatch::default();
        // Keys changed by the removed versions: delete those versions' value history and change records, and note
        // the keys whose latest value must be restored.
        let mut touched = BTreeSet::new();
        {
            let mut options = ReadOptions::default();
            options.set_iterate_lower_bound(first_removed.to_vec());
            let mut it = self.db.raw_iterator_cf_opt(change_log, options);
            it.seek(first_removed);
            while let Some((entry, _)) = it.item() {
                let (changed_at, key) = split_version(entry)?;
                batch.delete_cf(values, value_key(&key_hash(key), changed_at));
                batch.delete_cf(change_log, entry);
                touched.insert(key.to_vec());
                it.next();
            }
            it.status()?;
        }
        // The values of these keys at `version` (the `<= version` part of the value history is unaffected by this
        // deletion).
        for key in &touched {
            match read_value_at(&self.db, None, &key_hash(key), version)? {
                Some(value) => batch.put_cf(latest, key, value),
                None => batch.delete_cf(latest, key),
            }
        }
        // The version number comes first: the nodes created after `version`, the records that became stale only after
        // it and the roots after it are all contiguous ranges.
        batch.delete_range_cf(nodes, first_removed, VERSIONS_END);
        batch.delete_range_cf(stale, first_removed, VERSIONS_END);
        batch.delete_range_cf(roots, first_removed, VERSIONS_END);
        let meta = Meta {
            oldest: meta.oldest,
            latest: version,
        };
        batch.put_cf(meta_cf, META_KEY, meta.encode());

        self.write(batch)?;
        self.meta = Some(meta);
        Ok(())
    }

    /// Discards history, keeping only versions `>= keep_from` (still readable and able to be rolled back to).
    ///
    /// Deletes the JMT nodes no longer reachable as of `keep_from` (according to jmt's stale node index) and the
    /// superseded old values: for each key only the latest value among versions `<= keep_from` is kept (none at all
    /// when the latest one is a deletion).
    /// Does nothing when `keep_from <= oldest`; returns an error when `keep_from > latest`.
    pub fn prune(&mut self, keep_from: Version) -> Result<(), StateError> {
        let meta = self.meta.ok_or(StateError::Empty)?;
        if keep_from > meta.latest {
            return Err(StateError::VersionUnavailable {
                version: keep_from,
                oldest: meta.oldest,
                latest: meta.latest,
            });
        }
        if keep_from <= meta.oldest {
            return Ok(());
        }
        // The stale records and change records to process all have versions within [oldest, keep_from] (earlier ones
        // were deleted by previous prunings); the iteration starts at oldest, skipping the tombstones left by previous
        // prunings. keep_from <= latest < u64::MAX, cannot overflow.
        let processed_start = meta.oldest.to_be_bytes();
        let processed_end = keep_from
            .checked_add(1)
            .ok_or(StateError::VersionOutOfRange(keep_from))?
            .to_be_bytes();

        let nodes = cf_handle(&self.db, CF_NODES)?;
        let stale = cf_handle(&self.db, CF_STALE)?;
        let values = cf_handle(&self.db, CF_VALUES)?;
        let change_log = cf_handle(&self.db, CF_CHANGES)?;
        let roots = cf_handle(&self.db, CF_ROOTS)?;
        let meta_cf = cf_handle(&self.db, CF_META)?;

        let mut batch = WriteBatch::default();
        // 1. Nodes stale since `stale_since <= keep_from` are unreachable from the roots at `keep_from` and after.
        {
            let mut options = ReadOptions::default();
            options.set_iterate_range(processed_start.to_vec()..processed_end.to_vec());
            let mut it = self.db.raw_iterator_cf_opt(stale, options);
            it.seek(processed_start);
            while let Some((entry, _)) = it.item() {
                let (_, node_db_key) = split_version(entry)?;
                batch.delete_cf(nodes, node_db_key);
                batch.delete_cf(stale, entry);
                it.next();
            }
            it.status()?;
        }
        // 2. Superseded old values. If key K was changed at version u (<= keep_from), K's values before u are no
        //    longer visible to reads `>= keep_from`; if the record at u is a deletion, reads still see "absent" after
        //    deleting it (together with the earlier values).
        //    Only change records of versions `> oldest` remain (earlier ones were processed and deleted by previous
        //    prunings; the genesis version's records are kept until the first pruning).
        {
            let mut options = ReadOptions::default();
            options.set_iterate_range(processed_start.to_vec()..processed_end.to_vec());
            let mut it = self.db.raw_iterator_cf_opt(change_log, options);
            it.seek(processed_start);
            while let Some((entry, _)) = it.item() {
                let (changed_at, key) = split_version(entry)?;
                let hash = key_hash(key);
                if let Some(before) = changed_at.checked_sub(1) {
                    // Iterate only within this key_hash's history: all old values of versions `<= before`.
                    let mut options = ReadOptions::default();
                    options.set_iterate_range(PrefixRange(hash.as_slice()));
                    let mut history = self.db.raw_iterator_cf_opt(values, options);
                    history.seek(value_key(&hash, before));
                    while let Some((old, _)) = history.item() {
                        batch.delete_cf(values, old);
                        history.next();
                    }
                    history.status()?;
                }
                let at_change = value_key(&hash, changed_at);
                if let Some(stored) = self.db.get_pinned_cf(values, &at_change)? {
                    if decode_stored_value(&stored)?.is_none() {
                        batch.delete_cf(values, &at_change);
                    }
                }
                batch.delete_cf(change_log, entry);
                it.next();
            }
            it.status()?;
        }
        // 3. Roots before `keep_from` (exactly one per version; deleted one by one, so that every pruning does not
        //    leave behind a range-deletion tombstone).
        for version in meta.oldest..keep_from {
            batch.delete_cf(roots, version.to_be_bytes());
        }
        let meta = Meta {
            oldest: keep_from,
            latest: meta.latest,
        };
        batch.put_cf(meta_cf, META_KEY, meta.encode());

        self.write(batch)?;
        self.meta = Some(meta);
        Ok(())
    }

    /// Clears all data, returning to an empty database with no committed version (genesis can then be created again
    /// at any version).
    ///
    /// Used to rebuild the state from the genesis block when the old state has been pruned and the fork point cannot
    /// be rolled back to (the node's forced switch to the BTC-final chain, and crash recovery, D-093).
    /// One synchronous write batch: each column family is deleted by range up to past its largest key. Existing read
    /// views return [`StateError::StaleView`] on later reads (the root of their version no longer exists) and never
    /// read the rebuilt data.
    pub fn reset(&mut self) -> Result<(), StateError> {
        let mut batch = WriteBatch::default();
        for name in COLUMN_FAMILIES {
            let cf = cf_handle(&self.db, name)?;
            // The key formats differ between column families; the (exclusive) upper end is the largest key plus one
            // byte: every key is smaller than it
            let mut it = self.db.raw_iterator_cf(cf);
            it.seek_to_last();
            let Some((last, _)) = it.item() else {
                it.status()?;
                continue;
            };
            let mut end = last.to_vec();
            end.push(0);
            batch.delete_range_cf(cf, Vec::new(), end);
        }
        self.write(batch)?;
        self.meta = None;
        Ok(())
    }

    /// All key-values of the latest version whose full key starts with `prefix`, sorted by full key.
    /// A non-consensus auxiliary query (for the RPC, e.g. "all balances of an account"); the result is not paginated.
    #[allow(clippy::type_complexity)] // the agreed public signature
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StateError> {
        let cf = cf_handle(&self.db, CF_LATEST)?;
        let mut options = ReadOptions::default();
        options.set_iterate_range(PrefixRange(prefix));
        let mut it = self.db.raw_iterator_cf_opt(cf, options);
        it.seek(prefix);
        let mut entries = Vec::new();
        while let Some((key, value)) = it.item() {
            entries.push((key.to_vec(), value.to_vec()));
            it.next();
        }
        it.status()?;
        Ok(entries)
    }

    /// Read-only view of the latest version; returns an error for an empty database.
    pub fn view(&self) -> Result<StateView, StateError> {
        let latest = self.latest_version().ok_or(StateError::Empty)?;
        self.view_at(latest)
    }

    /// Read-only view of version `version`; returns an error when the version is not within `[oldest, latest]`.
    /// If that version is later rolled back or pruned, the view's reads return [`StateError::StaleView`].
    pub fn view_at(&self, version: Version) -> Result<StateView, StateError> {
        let root = self.root(version)?;
        Ok(StateView::new(Arc::clone(&self.db), version, root))
    }

    /// Checks that `version` is within the available range and returns the current metadata.
    fn check_available(&self, version: Version) -> Result<Meta, StateError> {
        let meta = self.meta.ok_or(StateError::Empty)?;
        if version < meta.oldest || version > meta.latest {
            return Err(StateError::VersionUnavailable {
                version,
                oldest: meta.oldest,
                latest: meta.latest,
            });
        }
        Ok(meta)
    }

    /// Checks that `version` can be the next committed version.
    fn check_next_version(&self, version: Version) -> Result<(), StateError> {
        if let Some(meta) = self.meta {
            let expected = meta
                .latest
                .checked_add(1)
                .ok_or(StateError::VersionOutOfRange(meta.latest))?;
            if version != expected {
                return Err(StateError::UnexpectedVersion {
                    expected,
                    got: version,
                });
            }
        }
        // jmt reserves u64::MAX as PRE_GENESIS_VERSION, and increments the "next version" after processing a version.
        if version == u64::MAX {
            return Err(StateError::VersionOutOfRange(version));
        }
        Ok(())
    }

    /// Filters out no-ops and computes the new root and the jmt update batch (without writing to the database).
    fn prepare<'w>(
        &self,
        version: Version,
        writes: &'w WriteSet,
    ) -> Result<Prepared<'w>, StateError> {
        let latest = cf_handle(&self.db, CF_LATEST)?;
        let mut changes = Vec::new();
        let mut value_set = Vec::new();
        for (key, value) in writes {
            let current = self.db.get_pinned_cf(latest, key)?;
            if current.as_deref() == value.as_deref() {
                continue;
            }
            changes.push((key.as_slice(), value.as_deref()));
            value_set.push((KeyHash(key_hash(key)), value.clone()));
        }
        let (root, update) = tree::apply(&self.db, self.latest_version(), version, value_set)?;
        Ok(Prepared {
            root,
            update,
            changes,
        })
    }

    fn write(&self, batch: WriteBatch) -> Result<(), StateError> {
        let mut options = WriteOptions::default();
        options.set_sync(true);
        self.db.write_opt(batch, &options)?;
        Ok(())
    }

    /// For tests: the underlying RocksDB.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &DB {
        &self.db
    }
}

impl StateRead for StateDb {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StateError> {
        StateDb::get(self, key)
    }
}

impl fmt::Debug for StateDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateDb")
            .field("path", &self.db.path())
            .field("oldest", &self.oldest_version())
            .field("latest", &self.latest_version())
            .finish()
    }
}

/// Gets a column family handle (all column families were created on open).
pub(crate) fn cf_handle<'a>(db: &'a DB, name: &str) -> Result<&'a ColumnFamily, StateError> {
    db.cf_handle(name)
        .ok_or_else(|| StateError::Corrupted(format!("missing column family {name}")))
}

/// The root of version `version` (the `roots` column family).
pub(crate) fn read_root(
    db: &DB,
    snapshot: Option<&SnapshotWithThreadMode<'_, DB>>,
    version: Version,
) -> Result<Option<[u8; 32]>, StateError> {
    let cf = cf_handle(db, CF_ROOTS)?;
    let stored = match snapshot {
        Some(snapshot) => snapshot.get_pinned_cf(cf, version.to_be_bytes())?,
        None => db.get_pinned_cf(cf, version.to_be_bytes())?,
    };
    match stored {
        None => Ok(None),
        Some(bytes) => <[u8; 32]>::try_from(bytes.as_ref()).map(Some).map_err(|_| {
            StateError::Corrupted(format!("root of version {version} has the wrong length"))
        }),
    }
}

/// The latest value of `key_hash` among versions `<= max_version` (one seek in the value history).
/// Does not check whether `max_version` is available; that is the caller's responsibility.
pub(crate) fn read_value_at(
    db: &DB,
    snapshot: Option<&SnapshotWithThreadMode<'_, DB>>,
    key_hash: &[u8; 32],
    max_version: Version,
) -> Result<Option<Vec<u8>>, StateError> {
    let cf = cf_handle(db, CF_VALUES)?;
    let mut options = ReadOptions::default();
    // The iteration range is limited to this key_hash's history, so the seek does not scan past it into other keys.
    options.set_iterate_range(PrefixRange(key_hash.as_slice()));
    let mut it = match snapshot {
        Some(snapshot) => snapshot.raw_iterator_cf_opt(cf, options),
        None => db.raw_iterator_cf_opt(cf, options),
    };
    it.seek(value_key(key_hash, max_version));
    match it.item() {
        Some((key, stored)) => {
            if key.len() != 40 {
                return Err(StateError::Corrupted(
                    "value history key has the wrong length".to_owned(),
                ));
            }
            decode_stored_value(stored)
        }
        None => {
            it.status()?;
            Ok(None)
        }
    }
}
