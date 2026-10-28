//! RocksDB memory bounds (D-103), shared by the databases of one process: the state store, and the node's block store
//! or the indexer's index. Every one of them is opened with [`DbMemory::open`].
//!
//! RocksDB's defaults suit one large database. Each column family gets a 64 MiB write buffer (memtable), written to
//! disk only once it is full, and a 32 MiB block cache of its own, and the index blocks of the table files stay in
//! memory outside any cache. A node writes a few KiB per block spread over 18 column families of two databases, so its
//! write buffers did not fill in days: they kept everything written since the process started, and the process grew
//! with the chain although pruning bounds the node's own structures (about 6 MiB per 1 000 blocks of one transfer
//! each).
//!
//! Here a column family's write buffer is flushed at [`CF_WRITE_BUFFER_BYTES`], a database's write buffers at
//! [`DB_WRITE_BUFFER_BYTES`] in all, and the column families of the process's databases share one block cache of
//! [`BLOCK_CACHE_BYTES`] that also holds the index blocks. These are local settings: they change neither what is stored
//! nor any result.

use std::path::Path;

use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, DBCompressionType, Options, DB,
    DEFAULT_COLUMN_FAMILY_NAME,
};

const MIB: usize = 1024 * 1024;

/// One column family's write buffer: flushed to disk once it holds this much.
pub const CF_WRITE_BUFFER_BYTES: usize = 4 * MIB;

/// All the write buffers of one database: once they hold this much, the column family with the oldest buffer is
/// flushed.
pub const DB_WRITE_BUFFER_BYTES: usize = 16 * MIB;

/// The block cache shared by the databases of one process, index blocks included.
pub const BLOCK_CACHE_BYTES: usize = 32 * MIB;

/// The RocksDB memory shared by the databases of one process: one block cache (clones share it).
#[derive(Clone)]
pub struct DbMemory {
    cache: Cache,
}

impl Default for DbMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl DbMemory {
    /// A block cache of [`BLOCK_CACHE_BYTES`] for the databases opened with it.
    pub fn new() -> Self {
        DbMemory {
            cache: Cache::new_lru_cache(BLOCK_CACHE_BYTES),
        }
    }

    /// Opens the database at `path` with the default column family and `column_families`, creating whatever does not
    /// exist yet, within these bounds. Nothing is compressed (the RocksDB build has no compression library).
    pub fn open(&self, path: &Path, column_families: &[&str]) -> Result<DB, rocksdb::Error> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        options.set_db_write_buffer_size(DB_WRITE_BUFFER_BYTES);
        options.set_compression_type(DBCompressionType::None);
        let descriptors = std::iter::once(DEFAULT_COLUMN_FAMILY_NAME)
            .chain(column_families.iter().copied())
            .map(|name| ColumnFamilyDescriptor::new(name, self.column_family_options()));
        DB::open_cf_descriptors(&options, path, descriptors)
    }

    /// A column family's options: its write buffer, and its blocks, index blocks included, in the shared cache (those
    /// of the newest table files pinned there, as RocksDB recommends with index blocks in the cache).
    fn column_family_options(&self) -> Options {
        let mut table = BlockBasedOptions::default();
        table.set_block_cache(&self.cache);
        table.set_cache_index_and_filter_blocks(true);
        table.set_pin_l0_filter_and_index_blocks_in_cache(true);
        let mut options = Options::default();
        options.set_write_buffer_size(CF_WRITE_BUFFER_BYTES);
        options.set_compression_type(DBCompressionType::None);
        options.set_block_based_table_factory(&table);
        options
    }

    /// The bytes the block cache holds.
    pub fn block_cache_bytes(&self) -> usize {
        self.cache.get_usage()
    }
}

/// The bytes the write buffers of `db` hold, those being flushed included: its default column family and
/// `column_families`.
pub fn memtable_bytes(db: &DB, column_families: &[&str]) -> Result<u64, rocksdb::Error> {
    let mut total: u64 = 0;
    for name in std::iter::once(DEFAULT_COLUMN_FAMILY_NAME).chain(column_families.iter().copied()) {
        if let Some(cf) = db.cf_handle(name) {
            let bytes = db
                .property_int_value_cf(cf, rocksdb::properties::CUR_SIZE_ALL_MEM_TABLES)?
                .unwrap_or(0);
            total = total.saturating_add(bytes);
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLUMN_FAMILIES: [&str; 3] = ["a", "b", "c"];

    fn key(salt: u8, batch: usize, index: usize) -> Vec<u8> {
        format!("{salt}/{batch}/{index}").into_bytes()
    }

    /// Writes `total` bytes in 1 MiB batches of 4 KiB values spread over the column families, and returns the most the
    /// write buffers held after any batch.
    fn write(db: &DB, total: usize, salt: u8) -> u64 {
        let mut most = 0;
        let value = vec![salt; 4096];
        for batch_index in 0..total / MIB {
            let mut batch = rocksdb::WriteBatch::default();
            for index in 0..MIB / 4096 {
                let cf = db
                    .cf_handle(COLUMN_FAMILIES[index % COLUMN_FAMILIES.len()])
                    .unwrap();
                batch.put_cf(cf, key(salt, batch_index, index), &value);
            }
            db.write(batch).unwrap();
            most = most.max(memtable_bytes(db, &COLUMN_FAMILIES).unwrap());
        }
        most
    }

    #[test]
    fn write_buffers_stay_within_the_database_bound() {
        let dir = tempfile::tempdir().unwrap();
        let memory = DbMemory::new();
        let db = memory.open(dir.path(), &COLUMN_FAMILIES).unwrap();
        // Four times the bound: with RocksDB's defaults (64 MiB per column family) all of it would stay in the write
        // buffers. Here they flush at 4 MiB per column family and 16 MiB in all; while one flushes, a column family
        // can fill another buffer, so they may briefly hold up to twice the bound
        let most = write(&db, 4 * DB_WRITE_BUFFER_BYTES, 1);
        let bound = 2 * DB_WRITE_BUFFER_BYTES as u64;
        assert!(
            most <= bound,
            "the write buffers held {most} bytes, more than {bound}"
        );
        // What was flushed reads back from the table files, through the block cache
        let before = memory.block_cache_bytes();
        let cf = db.cf_handle("a").unwrap();
        assert_eq!(db.get_cf(cf, key(1, 0, 0)).unwrap(), Some(vec![1; 4096]));
        assert!(memory.block_cache_bytes() > before);
    }

    #[test]
    fn the_databases_of_a_process_share_one_block_cache() {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let memory = DbMemory::new();
        let dbs: Vec<DB> = dirs
            .iter()
            .map(|dir| memory.open(dir.path(), &COLUMN_FAMILIES).unwrap())
            .collect();
        for (db, salt) in dbs.iter().zip([1, 2]) {
            write(db, MIB, salt);
            db.flush_cf(db.cf_handle("b").unwrap()).unwrap();
        }
        // A read from either database brings its block into the same cache
        let mut usage = memory.block_cache_bytes();
        for (db, salt) in dbs.iter().zip([1, 2]) {
            let cf = db.cf_handle("b").unwrap();
            assert_eq!(
                db.get_cf(cf, key(salt, 0, 1)).unwrap(),
                Some(vec![salt; 4096])
            );
            let now = memory.block_cache_bytes();
            assert!(
                now > usage,
                "a read from database {salt} did not use the shared cache"
            );
            usage = now;
        }
        assert!(usage <= BLOCK_CACHE_BYTES);
    }
}
