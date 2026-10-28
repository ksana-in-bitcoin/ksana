//! M10.2: compressed block headers in payloads (D-075).
//!
//! - `MAX_SETTLEMENT_HEADERS` is 500: the headers of a few hundred consecutive empty blocks all land on BTC with the
//!   next block that carries a settlement transaction, no republish needed; empty blocks do not change the state root
//!   (D-079) and take at most 18 bytes each;
//! - when the carried headers exceed the producer's size budget (half of `MAX_BLOCK_DATA_BYTES`), the earliest ones
//!   are dropped; after that block's settlement lands on BTC, the node republishes the dropped headers (the rescue
//!   republish rules, D-047, D-075) and the settled prefix advances as usual.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::mock_node::{signed, wait, MockNode};
use common::user;
use ksana_btc::client::BtcClient;
use ksana_consensus::Sha256d;
use ksana_primitives::{compact, BlockHeader, FeeMode, Params, Payload, DEVNET};
use ksana_rpc::SettlementState;

/// The payloads of the transactions in the mock chain's mempool.
fn mempool_payloads(n: &MockNode, params: &Params) -> Vec<Payload> {
    n.mock
        .mempool()
        .into_iter()
        .filter_map(|tx| ksana_btc::data::tx_payload(params, &tx).map(|(_, p)| p))
        .collect()
}

/// The header republish payloads in the mempool.
fn republished(n: &MockNode, params: &Params) -> Vec<Vec<BlockHeader>> {
    mempool_payloads(n, params)
        .into_iter()
        .filter_map(|p| match p {
            Payload::Headers { headers, .. } => Some(headers),
            _ => None,
        })
        .collect()
}

/// The payload of block `height`'s settlement transaction and its length.
fn settlement_payload(n: &MockNode, params: &Params, height: u64) -> (Vec<BlockHeader>, usize) {
    let txid = n
        .settlement(height)
        .txid
        .expect("the block carries a settlement transaction");
    let info = n.mock.transaction(&txid).unwrap().unwrap();
    let payload = ksana_btc::payload::envelope_payload(params, &info.tx).unwrap();
    match Payload::decode(&payload, params).unwrap() {
        Payload::Settlement { headers, .. } => (headers, payload.len()),
        other => panic!("not a settlement payload: {other:?}"),
    }
}

#[test]
fn a_long_run_of_empty_blocks_rides_on_the_next_settlement() {
    let (alice, bob) = (user("m10/payload/alice"), user("m10/payload/bob"));
    // A few hundred empty blocks: once the streak weight reaches 120 the network algorithm Argon2id takes about 0.25 s
    // per block (D-094), so the test algorithm Sha256d is used to produce blocks here
    let n = MockNode::start_with_algorithm(
        &[(&alice, 1_000)],
        Duration::from_millis(10),
        |mock| mock,
        Arc::new(Sha256d),
    );
    wait("enough empty blocks", || {
        (n.tip_height() >= 300).then_some(())
    });
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let block = n.wait_included(&n.submit(&op)).block.unwrap();
    assert!(
        block.height < 500,
        "the test assumes all empty blocks fit in one settlement payload"
    );
    n.wait_settlement(block.height, SettlementState::Broadcast);
    // The headers of heights 1..=block are all in this one payload. The first writes out every field, the last (this
    // block) carries the entries root and the new state root; the other empty blocks have the same state root as the
    // previous one and do not write it (D-079), at most 18 bytes each (14 bytes when bits are unchanged, 4 more when
    // ASERT adjusted the bits)
    let (headers, len) = settlement_payload(&n, &DEVNET, block.height);
    assert_eq!(
        headers.iter().map(|h| h.height).collect::<Vec<_>>(),
        (1..=block.height).collect::<Vec<_>>()
    );
    let mut encoder = compact::HeaderEncoder::default();
    let sizes: Vec<usize> = headers
        .iter()
        .map(|h| encoder.encode(h).unwrap().len())
        .collect();
    assert!(
        sizes[1..sizes.len() - 1].iter().all(|s| *s <= 18),
        "{sizes:?}"
    );
    // Including the entries, the whole payload is less than a tenth of the full header encoding
    let full: usize = headers.iter().map(BlockHeader::encoded_len).sum();
    assert!(len * 10 < full, "{len} vs {full}");
    n.mock.mine(1);
    n.wait_settlement(block.height, SettlementState::Confirmed);
    wait("prefix advances", || {
        n.settlement(block.height).settled.then_some(())
    });
    assert!(republished(&n, &DEVNET).is_empty(), "no republish needed");
}

#[test]
fn headers_beyond_the_payload_budget_are_republished() {
    // Payload limit 4 KiB: the producer carries at most about 2 KiB of other headers (about 14 bytes per empty block,
    // about 130 of them)
    let params = Params {
        max_block_data_bytes: 4 * 1024,
        ..DEVNET
    };
    let (alice, bob) = (user("m10/budget/alice"), user("m10/budget/bob"));
    // 200 empty blocks: with the network algorithm Argon2id they took most of the 60-second wait (D-094), so the test
    // algorithm Sha256d produces them, as in the test above
    let n = MockNode::start_with_params_and_algorithm(
        params.clone(),
        &[(&alice, 1_000)],
        Duration::from_millis(10),
        Arc::new(Sha256d),
    );
    wait("enough empty blocks", || {
        (n.tip_height() >= 200).then_some(())
    });
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let block = n.wait_included(&n.submit(&op)).block.unwrap();
    n.wait_settlement(block.height, SettlementState::Broadcast);
    let (headers, len) = settlement_payload(&n, &params, block.height);
    assert!(len <= params.max_block_data_bytes);
    // The earliest ones are dropped: what is carried is the run right next to this block
    let first = headers.first().unwrap().height;
    assert!(
        first > 1,
        "the earliest headers beyond the budget are dropped"
    );
    assert_eq!(
        headers.iter().map(|h| h.height).collect::<Vec<_>>(),
        (first..=block.height).collect::<Vec<_>>()
    );
    // Before the settlement lands on BTC, those headers may still be carried by another payload: no republish
    std::thread::sleep(Duration::from_millis(300));
    assert!(republished(&n, &params).is_empty());

    // After the settlement lands on BTC, the dropped headers are republished; once the republish lands, the settled
    // prefix moves past them
    n.mock.mine(1);
    n.wait_settlement(block.height, SettlementState::Confirmed);
    let headers = wait("header republish transaction", || {
        republished(&n, &params).pop()
    });
    assert_eq!(headers.first().unwrap().height, 1);
    assert_eq!(headers.last().unwrap().height, first - 1);
    n.mock.mine(1);
    wait("prefix advances", || {
        n.settlement(block.height).settled.then_some(())
    });
}
