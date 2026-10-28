//! Timestamp rules.
//!
//! - When receiving a new block (SPEC §6.3): greater than the median time of the previous 11 blocks, and not more
//!   than 15 seconds past the local time. These two are only checked when receiving a new block, not when syncing
//!   historical blocks; the local time is passed in by the caller.
//! - Consensus rule (D-083): not more than the time of the BTC block header pointed to by `btc_anchor` plus
//!   `MAX_TIMESTAMP_AHEAD_OF_ANCHOR`. The anchor hash commits to that block header, so syncing historical blocks and
//!   the indexer can both check it; the BTC block header's time is obtained by the caller from the BTC data source.

use ksana_primitives::{BlockHeader, Params};

use crate::ConsensusError;

/// The median of the timestamps (Bitcoin's convention: after sorting, take the element at index `len / 2`, i.e. the
/// larger middle value for an even count). Returns `None` for an empty slice. The input order does not matter.
pub fn median_time_past(timestamps: &[u64]) -> Option<u64> {
    let mut sorted = timestamps.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied()
}

/// The SPEC §6.3 check on receipt: `header.timestamp > median_time_past(recent)` and
/// `header.timestamp ≤ now_secs + max_future_block_time_secs`.
///
/// `recent_timestamps` are the timestamps of the most recent ancestors, ordered from oldest to newest (the parent
/// last); only the last `params.median_time_span` (11) are used, and near the genesis block, when there are fewer
/// than 11 ancestors, all of them are used. Must not be empty.
///
/// [`ConsensusError::TimestampTooFarInFuture`] does not mean the block is permanently invalid: it can be checked
/// again once the local time catches up.
pub fn check_timestamp(
    params: &Params,
    header: &BlockHeader,
    recent_timestamps: &[u64],
    now_secs: u64,
) -> Result<(), ConsensusError> {
    if params.median_time_span == 0 {
        return Err(ConsensusError::InvalidParams(
            "median_time_span must be greater than 0",
        ));
    }
    let skip = recent_timestamps
        .len()
        .saturating_sub(params.median_time_span);
    let window = recent_timestamps.get(skip..).unwrap_or_default();
    let median = median_time_past(window).ok_or(ConsensusError::NoRecentTimestamps)?;
    if header.timestamp <= median {
        return Err(ConsensusError::TimestampNotAfterMedian {
            median,
            got: header.timestamp,
        });
    }
    let max = now_secs.saturating_add(params.max_future_block_time_secs);
    if header.timestamp > max {
        return Err(ConsensusError::TimestampTooFarInFuture {
            max,
            got: header.timestamp,
        });
    }
    Ok(())
}

/// The D-083 consensus rule: `header.timestamp ≤ anchor_time + max_timestamp_ahead_of_anchor_secs`, where
/// `anchor_time` is the `time` field of the BTC block header pointed to by `header.btc_anchor`.
pub fn check_anchor_time(
    params: &Params,
    header: &BlockHeader,
    anchor_time: u32,
) -> Result<(), ConsensusError> {
    let max = u64::from(anchor_time).saturating_add(params.max_timestamp_ahead_of_anchor_secs);
    if header.timestamp > max {
        return Err(ConsensusError::TimestampAheadOfAnchor {
            max,
            got: header.timestamp,
        });
    }
    Ok(())
}
