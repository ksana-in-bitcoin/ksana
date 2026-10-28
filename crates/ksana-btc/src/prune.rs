//! Pruning policy and checkpoint files shared by the node and the indexer (D-097). Local policy and local formats, not
//! consensus: a node or indexer that prunes computes every block on, or merged by, a chain through the pruning root
//! exactly as one that never pruned.
//!
//! The pruning root R is the highest block decided BTC final at least [`Pruning::btc_margin`] BTC blocks below the BTC
//! tip: a BTC reorg up to that depth leaves it final, so every main chain still reachable passes through it. The
//! pruning point P is the block on R's selected chain `min_pruning_depth` blue score below it
//! ([`ksana_consensus::GhostdagParams::min_pruning_depth`]); the DAGs keep P and its future. Every pruning writes a
//! checkpoint ([`write_checkpoint`]) that a restart continues from ([`read_checkpoint`]).

use std::io::Write as _;
use std::path::Path;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_primitives::hash::sha256;

/// The default pruning margin in BTC blocks (about a day): the pruning root is the highest block decided BTC final at
/// least this many BTC blocks below the BTC tip. A BTC reorg up to this depth is followed as before; a deeper one
/// stops the node (resync) and makes the indexer rebuild from the genesis block.
pub const PRUNE_BTC_MARGIN: u32 = 144;

/// The default pruning step: prune again once the pruning point can advance by at least this much blue score.
pub const PRUNE_STEP: u64 = 1_000;

/// Pruning settings (D-097, local policy, not consensus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pruning {
    /// Whether to prune. Without pruning the whole block DAG, the main chain's summaries and the BTC snapshot stay in
    /// memory, as before D-097.
    pub enabled: bool,
    /// The pruning margin in BTC blocks ([`PRUNE_BTC_MARGIN`]).
    pub btc_margin: u32,
    /// The least blue score the pruning point advances by ([`PRUNE_STEP`]).
    pub step: u64,
}

impl Default for Pruning {
    fn default() -> Self {
        Pruning {
            enabled: true,
            btc_margin: PRUNE_BTC_MARGIN,
            step: PRUNE_STEP,
        }
    }
}

impl Pruning {
    /// No pruning.
    pub fn disabled() -> Self {
        Pruning {
            enabled: false,
            ..Pruning::default()
        }
    }
}

/// Writes a checkpoint file: the Borsh encoding of `value` followed by its SHA-256, to a temporary file first, synced,
/// then renamed over `path` and the directory synced, so that a crash leaves either the old or the new checkpoint.
/// Returns the number of bytes written.
pub fn write_checkpoint<T: BorshSerialize>(path: &Path, value: &T) -> std::io::Result<usize> {
    let mut bytes = borsh::to_vec(value)?;
    let digest = sha256(&bytes);
    bytes.extend_from_slice(&digest);
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(bytes.len())
}

/// Reads a checkpoint file written by [`write_checkpoint`]: `Ok(None)` when there is none; `Err` with the reason when
/// it is unreadable, truncated, corrupt (the SHA-256 does not match) or cannot be decoded.
pub fn read_checkpoint<T: BorshDeserialize>(path: &Path) -> Result<Option<T>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("unreadable ({e})")),
    };
    let Some((body, digest)) = bytes.split_last_chunk::<32>() else {
        return Err("truncated".into());
    };
    if sha256(body) != *digest {
        return Err("corrupt (checksum mismatch)".into());
    }
    borsh::from_slice(body)
        .map(Some)
        .map_err(|e| format!("undecodable ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checkpoint_round_trips_and_corruption_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pruning.ckpt");
        assert_eq!(read_checkpoint::<Vec<u64>>(&path), Ok(None));
        let value = vec![1u64, 2, 3];
        let written = write_checkpoint(&path, &value).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), written as u64);
        assert!(!path.with_extension("tmp").exists());
        assert_eq!(read_checkpoint::<Vec<u64>>(&path), Ok(Some(value.clone())));
        // A newer checkpoint replaces the older one
        write_checkpoint(&path, &vec![4u64]).unwrap();
        assert_eq!(read_checkpoint::<Vec<u64>>(&path), Ok(Some(vec![4])));

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_checkpoint::<Vec<u64>>(&path)
            .unwrap_err()
            .contains("checksum"));
        std::fs::write(&path, [0u8; 7]).unwrap();
        assert!(read_checkpoint::<Vec<u64>>(&path)
            .unwrap_err()
            .contains("truncated"));
    }
}
