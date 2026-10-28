//! Indexer metadata in the data directory (local format, not part of consensus): the genesis config fingerprint, the
//! genesis block header and the most recently verified block.
//!
//! The state store (`ksana-state`) only keeps the state itself; the indexer additionally records in `indexer.json`:
//! - the genesis config fingerprint: opening the same directory with a different genesis config is refused (same as the
//!   nodes, D-027);
//! - the genesis block header: still obtainable after version 0 of the state store has been pruned (the ASERT anchor
//!   and the starting point of BTC tracking);
//! - the most recently verified block (height, hash, state root): the status shown after a restart, before re-verifying
//!   against the BTC data.
//!
//! Writes go to a temporary file first and then rename it, so after a crash the content is either the old or the new
//! one. The state store is always written before the metadata: when the block in the metadata lags behind the latest
//! version of the state store, it is re-verified by state root against the BTC data after a restart (D-048).

use std::path::{Path, PathBuf};

use ksana_primitives::json::{dec_u64, hex_array};
use ksana_primitives::BlockHeader;
use serde::{Deserialize, Serialize};

use crate::IndexerError;

/// Metadata file name.
const FILE: &str = "indexer.json";

/// A verified block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TipRecord {
    /// Height.
    #[serde(with = "dec_u64")]
    pub height: u64,
    /// Block hash.
    #[serde(with = "hex_array")]
    pub hash: [u8; 32],
    /// State root.
    #[serde(with = "hex_array")]
    pub state_root: [u8; 32],
}

/// Metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    /// Fingerprint of the genesis config.
    #[serde(with = "hex_array")]
    pub fingerprint: [u8; 32],
    /// Genesis block header.
    pub genesis: BlockHeader,
    /// The most recently verified block.
    pub tip: TipRecord,
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

impl Meta {
    /// Loads; `None` when the file does not exist.
    pub fn load(data_dir: &Path) -> Result<Option<Meta>, IndexerError> {
        let file = path(data_dir);
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(IndexerError::Storage(format!(
                    "reading {}: {e}",
                    file.display()
                )))
            }
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| IndexerError::Storage(format!("parsing {}: {e}", file.display())))
    }

    /// Saves (temporary file + rename).
    pub fn save(&self, data_dir: &Path) -> Result<(), IndexerError> {
        let file = path(data_dir);
        let tmp = data_dir.join(format!("{FILE}.tmp"));
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| IndexerError::Internal(format!("encoding indexer metadata: {e}")))?;
        std::fs::write(&tmp, text)
            .map_err(|e| IndexerError::Storage(format!("writing {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &file)
            .map_err(|e| IndexerError::Storage(format!("writing {}: {e}", file.display())))
    }
}
