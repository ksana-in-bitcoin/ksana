//! M51: a header counts on BTC only once its parents have counted, and a body lands only with its counted header
//! (D-135). One node instance on the mock BTC chain, every block that needs a body carrying its own settlement
//! transactions, blocks produced by hand:
//!
//! - a child's settlement confirming before its parent's counts but lands nothing, and is not taken for landed: once
//!   the parent's has counted, the child's payload goes out again unchanged and lands the child's data, also when a
//!   later block's payload has counted the child's header in the meantime;
//! - a settlement that has not confirmed `IN_FLIGHT_BTC_BLOCKS` after its anchor no longer stops the next payload's
//!   search: that payload carries its header, so the next block's data lands without waiting for it, and its own body
//!   lands when it confirms after its header has counted.

mod common;

use bitcoin::hashes::Hash;
use bitcoin::Transaction;
use common::side::{setup, Side};
use common::{transfer_op, user};
use ksana_btc::mock::MockBtc;
use ksana_btc::payload::{attached_payload, envelope_payload};
use ksana_btc::settlement::SettlementPair;
use ksana_node::rescue::Rescuer;
use ksana_primitives::{Block, BlockHeader, Payload, DEVNET};

/// The block's settlement pair.
fn pair_of(block: &Block) -> SettlementPair {
    SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap()
}

/// The headers of the block's settlement payload.
fn payload_headers(block: &Block) -> Vec<BlockHeader> {
    let payload = attached_payload(&DEVNET, &pair_of(block)).unwrap();
    match Payload::decode(&payload, &DEVNET).unwrap() {
        Payload::Settlement { headers, .. } => headers,
        other => panic!("not a settlement payload: {other:?}"),
    }
}

/// The reveal transactions in the BTC mempool carrying exactly the block's settlement payload, other than its own.
fn sent_again(mock: &MockBtc, block: &Block) -> Vec<Transaction> {
    let pair = pair_of(block);
    let payload = attached_payload(&DEVNET, &pair).unwrap();
    mock.mempool()
        .into_iter()
        .filter(|tx| tx.compute_txid().to_byte_array() != pair.txid())
        .filter(|tx| envelope_payload(&DEVNET, tx).as_ref() == Some(&payload))
        .collect()
}

fn hold(mock: &MockBtc, pair: &SettlementPair) {
    mock.hold(&pair.commit_txid());
    mock.hold(&pair.txid());
}

fn release(mock: &MockBtc, pair: &SettlementPair) {
    mock.release(&pair.commit_txid());
    mock.release(&pair.txid());
}

#[test]
fn a_child_settlement_confirming_before_its_parents_goes_out_again_once_they_count() {
    let (alice, bob) = (user("m51/child/alice"), user("m51/child/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m51-child");
    a.submit(transfer_op(&alice, 0, 2, bob.account, 7));
    let parent = a.mine();
    a.submit(transfer_op(&alice, 1, 2, bob.account, 8));
    let child = a.mine();
    assert_eq!(child.header.parents, vec![parent.header.hash()]);
    // The parent's settlement is in flight: the child's payload carries the child's header alone
    assert_eq!(payload_headers(&child), vec![child.header.clone()]);
    let (parent_pair, child_pair) = (pair_of(&parent), pair_of(&child));

    // The parent's settlement waits in the BTC mempool while the child's confirms
    hold(&mock, &parent_pair);
    for _ in 0..DEVNET.settlement_depth {
        a.mine();
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(
        snapshot.ksn1_height(&child_pair.txid()).is_some(),
        "the child's payload counts"
    );
    assert_eq!(
        snapshot.counted_at(&child.header.hash()),
        None,
        "the child's header does not: its parent has not"
    );
    assert_eq!(snapshot.block_data_height(&child.header.hash()), None);
    assert_eq!(snapshot.data_landing(&child.header.hash()), None);
    // Nothing goes out again while the parent has not counted, and the settlement is not broadcast again
    a.chain.process_settlements().unwrap();
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    rescuer.run_once().unwrap();
    assert!(sent_again(&mock, &child).is_empty());

    // The parent's settlement confirms in the next BTC block. The child's, confirmed before it, landed nothing: it
    // goes out again unchanged, and lands the child's data
    release(&mock, &parent_pair);
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(
        snapshot.counted_at(&parent.header.hash()),
        Some(mock.tip_height())
    );
    assert_eq!(snapshot.counted_at(&child.header.hash()), None);
    a.chain.process_settlements().unwrap();
    rescuer.run_once().unwrap();
    assert_eq!(sent_again(&mock, &child).len(), 1, "sent again");
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    let height = mock.tip_height();
    assert_eq!(snapshot.counted_at(&child.header.hash()), Some(height));
    assert_eq!(
        snapshot.block_data_height(&child.header.hash()),
        Some(height)
    );
    assert_eq!(
        a.chain
            .carried_on_chain(
                &a.chain
                    .store()
                    .block(&child.header.hash())
                    .unwrap()
                    .unwrap(),
                &snapshot
            )
            .unwrap(),
        Some(height)
    );
    rescuer.run_once().unwrap();
    assert_eq!(sent_again(&mock, &child).len(), 0, "nothing more to send");
}

#[test]
fn a_child_settlement_goes_out_again_also_once_a_later_payload_counted_its_header() {
    let (alice, bob) = (user("m51/later/alice"), user("m51/later/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m51-later");
    a.submit(transfer_op(&alice, 0, 2, bob.account, 7));
    let parent = a.mine();
    a.submit(transfer_op(&alice, 1, 2, bob.account, 8));
    let child = a.mine();
    let (parent_pair, child_pair) = (pair_of(&parent), pair_of(&child));

    // The child's settlement confirms a BTC block before the parent's, and lands nothing
    hold(&mock, &parent_pair);
    for _ in 0..DEVNET.settlement_depth {
        a.mine();
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    let confirmed = mock.tip_height();
    release(&mock, &parent_pair);
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.ksn1_height(&child_pair.txid()), Some(confirmed));
    assert_eq!(
        snapshot.counted_at(&parent.header.hash()),
        Some(confirmed + 1)
    );
    assert_eq!(snapshot.counted_at(&child.header.hash()), None);

    // Before the child's payload goes out again, the payload of a later block counts the child's header: the child's
    // settlement no longer in flight, that payload carries its header, not its body
    mock.mine(1);
    a.sync();
    a.submit(transfer_op(&alice, 2, 2, bob.account, 9));
    let later = loop {
        let block = a.mine();
        if !block.entries.is_empty() {
            break block;
        }
    };
    assert!(payload_headers(&later).contains(&child.header));
    for _ in 0..DEVNET.settlement_depth {
        a.mine();
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    let height = mock.tip_height();
    assert_eq!(snapshot.counted_at(&child.header.hash()), Some(height));
    assert_eq!(
        snapshot.block_data_height(&later.header.hash()),
        Some(height)
    );
    assert_eq!(
        snapshot.block_data_height(&child.header.hash()),
        None,
        "the child's own payload confirmed below the height at which its header counted"
    );

    // The child's payload goes out again all the same, and lands its body
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    rescuer.run_once().unwrap();
    assert_eq!(sent_again(&mock, &child).len(), 1, "sent again");
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(
        snapshot.block_data_height(&child.header.hash()),
        Some(height + 1)
    );
    rescuer.run_once().unwrap();
    assert!(sent_again(&mock, &child).is_empty(), "nothing more to send");
}

#[test]
fn a_settlement_late_by_more_than_the_in_flight_window_has_its_header_carried_by_the_next() {
    let (alice, bob) = (user("m51/late/alice"), user("m51/late/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m51-late");
    a.submit(transfer_op(&alice, 0, 2, bob.account, 7));
    let late = a.mine();
    let late_pair = pair_of(&late);
    hold(&mock, &late_pair);
    for _ in 0..DEVNET.settlement_depth {
        a.mine();
    }
    a.chain.process_settlements().unwrap();
    // Three BTC blocks pass without it: more than IN_FLIGHT_BTC_BLOCKS (2) after its anchor
    mock.mine(3);
    a.sync();
    a.submit(transfer_op(&alice, 1, 2, bob.account, 8));
    let next = loop {
        let block = a.mine();
        if !block.entries.is_empty() {
            break block;
        }
    };
    // The next payload carries the late block's header (not its body) and the headers of the empty blocks between them
    let carried = payload_headers(&next);
    assert_eq!(carried.first(), Some(&late.header));
    assert_eq!(carried.last(), Some(&next.header));
    let next_pair = pair_of(&next);
    for _ in 0..DEVNET.settlement_depth {
        a.mine();
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    let height = mock.tip_height();
    assert!(snapshot.ksn1_height(&next_pair.txid()).is_some());
    assert_eq!(snapshot.counted_at(&late.header.hash()), Some(height));
    assert_eq!(snapshot.counted_at(&next.header.hash()), Some(height));
    assert_eq!(
        snapshot.block_data_height(&next.header.hash()),
        Some(height),
        "the next block's data lands without waiting for the late settlement"
    );
    assert_eq!(
        snapshot.block_data_height(&late.header.hash()),
        None,
        "the late block's body has not landed"
    );

    // The late settlement confirms: its header has counted already, and its body lands
    release(&mock, &late_pair);
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.counted_at(&late.header.hash()), Some(height));
    assert_eq!(
        snapshot.block_data_height(&late.header.hash()),
        Some(height + 1)
    );
}
