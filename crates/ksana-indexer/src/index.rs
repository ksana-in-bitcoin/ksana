//! The indexer's index of the executed main chain (D-097, local format, not part of consensus), a RocksDB in the
//! `index` subdirectory of the data directory:
//!
//! | Column family | Key | Value |
//! | --- | --- | --- |
//! | `main` | height (8 bytes big-endian) | Borsh(`(MainEntry, state root)`): the summary of the executed main-chain block at that height and of the blocks it merges, and the verified state root |
//! | `accepted` | block hash | Borsh(`(u64, BlockSummary)`): the height of the main-chain block that accepted the block, and its summary |
//! | `verdicts` | block hash | the version of the verdict rules (1 byte, `ksana_btc::verdict::VERDICT_VERSION`), then Borsh(`(bool, [u8; 32])`): the verdict of executing the block on top of its selected parent (valid or not) and the BTC block hash it was given on (D-101; a cache, so that a restart does not judge the blocks again; one kept under another version is judged again) |
//!
//! Memory holds only a window of the executed main chain (`ksana_btc::tracker::MainChain`); this index answers for
//! everything below it: the blocks accepted there (the carrier rule may ask about any accepted block, D-095), the
//! executed blocks shown by the RPC, and the window itself after a restart. An entry is written before the state of
//! its height is committed, and the entries above a height are deleted when the state is rolled back below them, so
//! every entry at or below the state's latest version describes the executed main chain (entries above it are
//! leftovers of an unfinished step, deleted at the next start).

use std::path::Path;
use std::sync::Arc;

use ksana_btc::tracker::{BlockSummary, MainEntry, MainHistory};
use ksana_btc::verdict;
use ksana_state::rocks::{self, DbMemory};
use rocksdb::{WriteBatch, DB};

use crate::IndexerError;

const CF_MAIN: &str = "main";
const CF_ACCEPTED: &str = "accepted";
const CF_VERDICTS: &str = "verdicts";

const COLUMN_FAMILIES: [&str; 3] = [CF_MAIN, CF_ACCEPTED, CF_VERDICTS];

fn storage(e: impl std::fmt::Display) -> IndexerError {
    IndexerError::Storage(format!("index: {e}"))
}

fn decode<T: borsh::BorshDeserialize>(bytes: &[u8]) -> Result<T, IndexerError> {
    borsh::from_slice(bytes).map_err(|e| storage(format!("stored data is corrupt: {e}")))
}

fn encode<T: borsh::BorshSerialize>(value: &T) -> Result<Vec<u8>, IndexerError> {
    borsh::to_vec(value).map_err(|e| IndexerError::Internal(e.to_string()))
}

/// The index of the executed main chain.
pub struct Index {
    db: DB,
}

impl Index {
    /// Opens (creates when it does not exist), within the RocksDB memory bounds of `memory`, whose block cache it shares
    /// with the state store (D-103).
    pub fn open(path: &Path, memory: &DbMemory) -> Result<Self, IndexerError> {
        let db = memory.open(path, &COLUMN_FAMILIES).map_err(storage)?;
        Ok(Index { db })
    }

    /// The bytes RocksDB's write buffers of the index hold (D-103).
    pub fn memtable_bytes(&self) -> Result<u64, IndexerError> {
        rocks::memtable_bytes(&self.db, &COLUMN_FAMILIES).map_err(storage)
    }

    fn cf(&self, name: &str) -> Result<&rocksdb::ColumnFamily, IndexerError> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| storage(format!("missing column family {name}")))
    }

    /// Records the executed main-chain block at `height` (and the blocks it accepts) with its verified state root. An
    /// entry already at that height is replaced together with the acceptance records it left there (another main chain
    /// with the same state roots, e.g. empty blocks, when verifying after a restart).
    pub fn put(
        &self,
        height: u64,
        entry: &MainEntry,
        state_root: [u8; 32],
    ) -> Result<(), IndexerError> {
        let main = self.cf(CF_MAIN)?;
        let accepted = self.cf(CF_ACCEPTED)?;
        let mut batch = WriteBatch::default();
        if let Some((old, _)) = self.get(height)? {
            for block in old.accepted() {
                if self
                    .accepted(&block.hash)?
                    .is_some_and(|(at, _)| at == height)
                {
                    batch.delete_cf(accepted, block.hash);
                }
            }
        }
        batch.put_cf(main, height.to_be_bytes(), encode(&(entry, state_root))?);
        for block in entry.accepted() {
            batch.put_cf(accepted, block.hash, encode(&(height, block))?);
        }
        self.db.write(batch).map_err(storage)
    }

    /// The executed main-chain block at `height` and its state root.
    pub fn get(&self, height: u64) -> Result<Option<(MainEntry, [u8; 32])>, IndexerError> {
        self.db
            .get_cf(self.cf(CF_MAIN)?, height.to_be_bytes())
            .map_err(storage)?
            .map(|bytes| decode(&bytes))
            .transpose()
    }

    /// The height of the executed main-chain block that accepted block `hash`, and its summary.
    pub fn accepted(&self, hash: &[u8; 32]) -> Result<Option<(u64, BlockSummary)>, IndexerError> {
        self.db
            .get_cf(self.cf(CF_ACCEPTED)?, hash)
            .map_err(storage)?
            .map(|bytes| decode(&bytes))
            .transpose()
    }

    /// The verdict on a block kept on disk (D-101): valid or not, and the basis it was given on.
    pub fn verdict(&self, hash: &[u8; 32]) -> Result<Option<(bool, [u8; 32])>, IndexerError> {
        Ok(self
            .db
            .get_cf(self.cf(CF_VERDICTS)?, hash)
            .map_err(storage)?
            .and_then(|bytes| verdict::decode_stored(&bytes)))
    }

    /// Keeps the verdict on a block given on `basis` (D-101).
    pub fn put_verdict(
        &self,
        hash: &[u8; 32],
        valid: bool,
        basis: &[u8; 32],
    ) -> Result<(), IndexerError> {
        self.db
            .put_cf(
                self.cf(CF_VERDICTS)?,
                hash,
                verdict::encode_stored(valid, basis),
            )
            .map_err(storage)
    }

    /// Deletes the entries above height `to`, with the acceptance records of the blocks they accept.
    pub fn rewind(&self, to: u64) -> Result<(), IndexerError> {
        let Some(from) = to.checked_add(1) else {
            return Ok(());
        };
        let main = self.cf(CF_MAIN)?;
        let accepted = self.cf(CF_ACCEPTED)?;
        let mut batch = WriteBatch::default();
        let start = from.to_be_bytes();
        for item in self.db.iterator_cf(
            main,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        ) {
            let (key, value) = item.map_err(storage)?;
            let (entry, _): (MainEntry, [u8; 32]) = decode(&value)?;
            for block in entry.accepted() {
                // The block may have been accepted again at or below `to` (a main-chain switch)
                let here = self
                    .accepted(&block.hash)?
                    .is_some_and(|(height, _)| height >= from);
                if here {
                    batch.delete_cf(accepted, block.hash);
                }
            }
            batch.delete_cf(main, key);
        }
        self.db.write(batch).map_err(storage)
    }

    /// Deletes everything (rebuilding from the genesis block).
    pub fn clear(&self) -> Result<(), IndexerError> {
        let mut batch = WriteBatch::default();
        for name in [CF_MAIN, CF_ACCEPTED, CF_VERDICTS] {
            let cf = self.cf(name)?;
            for item in self.db.iterator_cf(cf, rocksdb::IteratorMode::Start) {
                let (key, _) = item.map_err(storage)?;
                batch.delete_cf(cf, key);
            }
        }
        self.db.write(batch).map_err(storage)
    }
}

/// The main chain below the in-memory window, answered from the index (D-097).
pub struct IndexHistory(pub Arc<Index>);

impl MainHistory for IndexHistory {
    fn accepted(&self, hash: &[u8; 32]) -> Result<Option<(u64, BlockSummary)>, String> {
        self.0.accepted(hash).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(hash: u8) -> BlockSummary {
        BlockSummary {
            hash: [hash; 32],
            settles: true,
            anchor_height: 7,
            producer: [9; 32],
            daa_score: u64::from(hash),
            weight: 1,
            rewarded: true,
        }
    }

    fn entry(hash: u8, merged: &[u8]) -> MainEntry {
        let own = summary(hash);
        MainEntry {
            hash: own.hash,
            settles: own.settles,
            anchor_height: own.anchor_height,
            producer: own.producer,
            daa_score: own.daa_score,
            weight: own.weight,
            merged: merged.iter().map(|m| summary(*m)).collect(),
        }
    }

    #[test]
    fn entries_are_written_rewound_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path(), &DbMemory::new()).unwrap();
        index.put(1, &entry(1, &[]), [1; 32]).unwrap();
        index.put(2, &entry(2, &[3]), [2; 32]).unwrap();
        assert_eq!(index.get(2).unwrap(), Some((entry(2, &[3]), [2; 32])));
        assert_eq!(index.accepted(&[3; 32]).unwrap(), Some((2, summary(3))));

        // Block 3 is accepted again at height 1 by another main chain: rewinding above 1 keeps that record
        index.rewind(0).unwrap();
        index.put(1, &entry(4, &[3]), [4; 32]).unwrap();
        index.put(2, &entry(5, &[]), [5; 32]).unwrap();
        index.rewind(1).unwrap();
        assert_eq!(index.get(2).unwrap(), None);
        assert_eq!(index.accepted(&[5; 32]).unwrap(), None);
        assert_eq!(index.accepted(&[3; 32]).unwrap(), Some((1, summary(3))));
        assert_eq!(index.accepted(&[1; 32]).unwrap(), None);

        // Replacing the entry at a height drops the acceptance records it left there, and only those
        index.put(2, &entry(6, &[7]), [6; 32]).unwrap();
        index.put(3, &entry(8, &[3]), [8; 32]).unwrap();
        index.put(2, &entry(9, &[]), [9; 32]).unwrap();
        index.put(1, &entry(4, &[]), [4; 32]).unwrap();
        assert_eq!(index.accepted(&[6; 32]).unwrap(), None);
        assert_eq!(index.accepted(&[7; 32]).unwrap(), None);
        assert_eq!(index.accepted(&[9; 32]).unwrap(), Some((2, summary(9))));
        assert_eq!(index.accepted(&[3; 32]).unwrap(), Some((3, summary(3))));
        assert_eq!(index.accepted(&[4; 32]).unwrap(), Some((1, summary(4))));

        index.clear().unwrap();
        assert_eq!(index.get(1).unwrap(), None);
        assert_eq!(index.accepted(&[3; 32]).unwrap(), None);
    }
}
