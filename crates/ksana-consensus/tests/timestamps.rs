//! The timestamp rules when receiving a new block (SPEC §6.3): greater than the median time of the previous 11 blocks,
//! not more than local time + 15 seconds.

use ksana_consensus::{check_timestamp, median_time_past, ConsensusError};
use ksana_primitives::{BlockHeader, BtcAnchor, Params, DEVNET};
use proptest::prelude::*;

fn header(timestamp: u64) -> BlockHeader {
    BlockHeader {
        version: 1,
        height: 12,
        parents: vec![[1; 32]],
        btc_anchor: BtcAnchor {
            height: 1,
            hash: [2; 32],
        },
        entries_root: [3; 32],
        state_root: [4; 32],
        producer: [5; 32],
        timestamp,
        bits: DEVNET.pow_limit_bits,
        nonce: 0,
    }
}

#[test]
fn median_follows_bitcoin_convention() {
    assert_eq!(median_time_past(&[]), None);
    assert_eq!(median_time_past(&[7]), Some(7));
    assert_eq!(median_time_past(&[3, 1, 2]), Some(2));
    // an even count: after sorting, take index len / 2, i.e. the larger of the two middle values
    assert_eq!(median_time_past(&[1, 2]), Some(2));
    assert_eq!(median_time_past(&[4, 1, 3, 2]), Some(3));
    assert_eq!(median_time_past(&[5, 5, 5, 1, 9]), Some(5));
    let eleven = [100, 90, 110, 95, 105, 120, 80, 101, 99, 130, 70];
    assert_eq!(median_time_past(&eleven), Some(100));
    assert_eq!(median_time_past(&[u64::MAX, 0, u64::MAX]), Some(u64::MAX));
}

proptest! {
    /// Agrees with an independent formulation (counting one by one), and is independent of the input order.
    #[test]
    fn median_is_order_independent(mut values in prop::collection::vec(any::<u64>(), 1..30)) {
        let median = median_time_past(&values).unwrap();
        // exactly len/2 elements come before the median's position: at most len/2 are strictly smaller, at least
        // len/2 + 1 are ≤
        let below = values.iter().filter(|&&v| v < median).count();
        let at_most = values.iter().filter(|&&v| v <= median).count();
        prop_assert!(below <= values.len() / 2 && at_most > values.len() / 2);
        values.reverse();
        prop_assert_eq!(median_time_past(&values), Some(median));
        let third = values.len() / 3;
        values.rotate_left(third);
        prop_assert_eq!(median_time_past(&values), Some(median));
    }
}

#[test]
fn timestamp_must_exceed_the_median() {
    let recent = [1000, 1010, 1005, 1020, 1015]; // median 1010
    let now = 1030;
    assert_eq!(
        check_timestamp(&DEVNET, &header(1010), &recent, now),
        Err(ConsensusError::TimestampNotAfterMedian {
            median: 1010,
            got: 1010
        })
    );
    assert_eq!(
        check_timestamp(&DEVNET, &header(900), &recent, now),
        Err(ConsensusError::TimestampNotAfterMedian {
            median: 1010,
            got: 900
        })
    );
    assert_eq!(
        check_timestamp(&DEVNET, &header(1011), &recent, now),
        Ok(())
    );
    // may be earlier than the parent (as long as it exceeds the median)
    assert_eq!(
        check_timestamp(&DEVNET, &header(1012), &recent, now),
        Ok(())
    );
    // near the genesis block: only one ancestor
    assert_eq!(check_timestamp(&DEVNET, &header(1), &[0], now), Ok(()));
    assert_eq!(
        check_timestamp(&DEVNET, &header(0), &[0], now),
        Err(ConsensusError::TimestampNotAfterMedian { median: 0, got: 0 })
    );
    assert_eq!(
        check_timestamp(&DEVNET, &header(5), &[], now),
        Err(ConsensusError::NoRecentTimestamps)
    );
}

#[test]
fn timestamp_may_be_at_most_15_seconds_ahead_of_local_time() {
    assert_eq!(DEVNET.max_future_block_time_secs, 15);
    let recent = [100];
    let now = 1_000;
    assert_eq!(
        check_timestamp(&DEVNET, &header(1_015), &recent, now),
        Ok(())
    );
    assert_eq!(
        check_timestamp(&DEVNET, &header(1_016), &recent, now),
        Err(ConsensusError::TimestampTooFarInFuture {
            max: 1_015,
            got: 1_016
        })
    );
    // the same block passes once the local time has caught up
    assert_eq!(
        check_timestamp(&DEVNET, &header(1_016), &recent, now + 1),
        Ok(())
    );
    // no overflow when the local time is close to u64::MAX
    assert_eq!(
        check_timestamp(&DEVNET, &header(u64::MAX), &recent, u64::MAX - 3),
        Ok(())
    );
}

/// Only the last `median_time_span` (11) timestamps are used (ordered from oldest to newest).
#[test]
fn only_the_last_eleven_ancestors_count() {
    assert_eq!(DEVNET.median_time_span, 11);
    // the oldest one is abnormally large; the last 11 are 100..=110, median 105. If all 12 counted, the median would
    // be 106.
    let mut recent = vec![1_000_000];
    recent.extend(100..=110);
    assert_eq!(median_time_past(&recent), Some(106));
    assert_eq!(check_timestamp(&DEVNET, &header(106), &recent, 200), Ok(()));
    assert_eq!(
        check_timestamp(&DEVNET, &header(105), &recent, 200),
        Err(ConsensusError::TimestampNotAfterMedian {
            median: 105,
            got: 105
        })
    );
    // other window lengths follow the params
    let params = Params {
        median_time_span: 3,
        ..DEVNET
    };
    // the last 3: 108, 109, 110, median 109
    assert_eq!(check_timestamp(&params, &header(110), &recent, 200), Ok(()));
    assert_eq!(
        check_timestamp(&params, &header(109), &recent, 200),
        Err(ConsensusError::TimestampNotAfterMedian {
            median: 109,
            got: 109
        })
    );
    // without the last one: 107, 108, 109, median 108
    assert_eq!(
        check_timestamp(&params, &header(109), &recent[..recent.len() - 1], 200),
        Ok(())
    );
    assert_eq!(
        check_timestamp(&params, &header(108), &recent[..recent.len() - 1], 200),
        Err(ConsensusError::TimestampNotAfterMedian {
            median: 108,
            got: 108
        })
    );
    let params = Params {
        median_time_span: 0,
        ..DEVNET
    };
    assert!(matches!(
        check_timestamp(&params, &header(109), &recent, 200),
        Err(ConsensusError::InvalidParams(_))
    ));
}
