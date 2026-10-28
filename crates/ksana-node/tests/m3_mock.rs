//! Node behavior of M3 (in-process node + in-memory mock BTC chain `MockBtc`, no bitcoind needed):
//! the anchor follows the BTC tip, delayed broadcast of settlement transactions, forced inclusion of native
//! transactions, unlock entries, and after a BTC reorg the fast-lane state is unchanged and the settlement lands on BTC
//! again. The acceptance tests on real bitcoind are in `tests/integration/tests/m3_regtest.rs`.

mod common;

use std::time::Duration;

use bitcoin::hashes::Hash;
use common::mock_node::{native_tx, signed, signed_native, wait, MockNode};
use common::{chain_slots, user};
use ksana_btc::client::BtcClient;
use ksana_btc::settlement::SettlementPair;
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_primitives::{BtcAnchor, Entry, FeeMode, DEVNET};
use ksana_rpc::{OpState, SettlementState};

#[test]
fn anchor_follows_the_btc_tip() {
    let n = MockNode::start(&[]);
    wait("first block", || (n.tip_height() >= 1).then_some(()));
    let first = n.block(n.tip_height());
    assert_eq!(first.header.btc_anchor.height, n.mock.tip_height());
    let mined = n.mock.mine(2);
    let expected = BtcAnchor {
        height: n.mock.tip_height(),
        hash: *mined.last().unwrap(),
    };
    wait("anchor at the new BTC tip", || {
        (n.block(n.tip_height()).header.btc_anchor == expected).then_some(())
    });
    // The anchor height never decreases from block to block
    let tip = n.tip_height();
    for h in 1..=tip {
        let (a, b) = (n.block(h - 1), n.block(h));
        assert!(b.header.btc_anchor.height >= a.header.btc_anchor.height);
    }
}

#[test]
fn settlement_is_broadcast_after_the_settlement_depth_and_confirms() {
    let alice = user("m3/alice");
    let bob = user("m3/bob");
    let n = MockNode::start(&[(&alice, 1_000)]);
    let op = signed(&alice, 0, FeeMode::Token { amount: 3 }, bob.account, 7);
    let op_id = n.submit(&op);
    let status = n.wait_included(&op_id);
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    assert_eq!(n.ksa(&bob), 7);
    let height = status.block.unwrap().height;

    // Not broadcast while fast-lane confirmations are below SETTLEMENT_DEPTH
    let s = n.settlement(height);
    let txid = s.txid.expect("the block carries a settlement transaction");
    if n.tip_height() < height + DEVNET.settlement_depth {
        assert_eq!(s.status, SettlementState::Pending);
        assert!(!n.mock.in_mempool(&txid).unwrap());
    }
    let s = n.wait_settlement(height, SettlementState::Broadcast);
    assert!(n.tip_height() >= height + DEVNET.settlement_depth);
    assert!(n.mock.in_mempool(&txid).unwrap());
    let pair = SettlementPair::decode(
        s.txs
            .as_ref()
            .expect("the block carries a settlement transaction"),
    )
    .unwrap();
    // The commit transaction's change returns to the producer's funding address; the settlement txid is the reveal
    // transaction (which carries the payload, D-081)
    assert_eq!(pair.commit.output[0].script_pubkey, n.funding.script);
    assert_eq!(pair.txid(), txid);
    assert_eq!(s.commit_txid, Some(pair.commit_txid()));
    assert!(n.mock.in_mempool(&pair.commit_txid()).unwrap());
    n.mock.mine(1);
    let s = n.wait_settlement(height, SettlementState::Confirmed);
    assert_eq!(s.confirmed.unwrap().txid, txid);
}

#[test]
fn native_op_is_forced_at_the_start_of_the_next_block() {
    let carol = user("m3/carol");
    let dave = user("m3/dave");
    let n = MockNode::start(&[(&carol, 100)]);
    wait("first block", || (n.tip_height() >= 1).then_some(()));

    let (native, op) = native_tx(&n.mock, |input| {
        signed_native(&carol, 0, input, dave.account, 9)
    });
    n.mock.mine_block(vec![native.clone()]).unwrap();
    let confirmed_at = n.mock.tip_height();

    let status = n.wait_included(&op.op_id().unwrap());
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    assert_eq!(status.index, Some(0));
    assert_eq!(n.ksa(&dave), 9);
    let height = status.block.unwrap().height;
    let block = n.block(height);
    assert!(
        matches!(&block.entries[0], Entry::Forced { btc_txid, .. } if *btc_txid == native.compute_txid().to_byte_array())
    );
    assert!(block.header.btc_anchor.height >= confirmed_at);
    assert!(n.block(height - 1).header.btc_anchor.height < confirmed_at);
    // Later blocks do not include it again
    wait("two more blocks", || {
        (n.tip_height() >= height + 2).then_some(())
    });
    for h in height + 1..=n.tip_height() {
        assert!(!n
            .block(h)
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Forced { .. })));
    }
}

#[test]
fn token_fee_unlocks_after_the_settlement_confirms() {
    let erin = user("m3/erin");
    let frank = user("m3/frank");
    let n = MockNode::start(&[(&erin, 1_000)]);
    let op = signed(&erin, 0, FeeMode::Token { amount: 17 }, frank.account, 1);
    let op_id = n.submit(&op);
    let block = n.wait_included(&op_id).block.unwrap();
    n.wait_settlement(block.height, SettlementState::Broadcast);
    // Settlement unconfirmed: no unlock
    wait("two more blocks", || {
        (n.tip_height() >= block.height + 4).then_some(())
    });
    assert_eq!(n.ksa(&n.producer), 0);
    n.mock.mine(1);
    n.wait_settlement(block.height, SettlementState::Confirmed);
    // Cumulative unlock up to that block's acceptance position (D-074, D-079): the headers of the empty blocks before
    // it are carried by its settlement payload and enter the settled closure together; what is unlocked is the
    // sponsored fee and the block rewards of these blocks (paid out at unlock)
    let released = wait("unlock", || {
        let balance = n.ksa(&n.producer);
        (balance > 0).then_some(balance)
    });
    // Each block's number of reward slots is its weight (empty blocks 12, 24, ... by streak, D-084, D-090)
    let slots = chain_slots(block.height, |h| n.block(h).header);
    assert_eq!(released, 17 + slots * DEVNET.block_reward);
    let tip = n.tip_height();
    let unlocks: Vec<Entry> = (block.height + 1..=tip)
        .flat_map(|h| n.block(h).entries)
        .filter(|e| matches!(e, Entry::Unlock { .. }))
        .collect();
    assert_eq!(
        unlocks.len(),
        1,
        "the same block is unlocked only once: {unlocks:?}"
    );
    // A single producer and no merged blocks: a block's acceptance position is (height, 0)
    assert!(matches!(
        &unlocks[0],
        Entry::Unlock { height, index: 0 } if *height == block.height
    ));
}

#[test]
fn btc_reorg_keeps_fast_state_and_the_settlement_confirms_again() {
    let gina = user("m3/gina");
    let hank = user("m3/hank");
    let n = MockNode::start(&[(&gina, 1_000)]);
    let op = signed(&gina, 0, FeeMode::Token { amount: 3 }, hank.account, 5);
    let op_id = n.submit(&op);
    let block = n.wait_included(&op_id).block.unwrap();
    let s = n.wait_settlement(block.height, SettlementState::Broadcast);
    n.mock.mine(1);
    n.wait_settlement(block.height, SettlementState::Confirmed);
    let fast_height = n.tip_height();

    // A BTC reorg removes the block confirming the settlement: the block it settles, anchored below it, does not roll
    // back (the blocks anchored on the removed block leave the main chain, D-104), and the settlement returns to the
    // mempool and confirms again once mined again
    n.mock.reorg(1);
    n.mock.mine(1);
    wait("fast lane keeps producing blocks", || {
        (n.tip_height() >= fast_height + 2).then_some(())
    });
    assert_eq!(n.ksa(&hank), 5);
    assert_eq!(
        n.block(block.height).hash,
        block.hash,
        "the fast lane does not roll back"
    );
    n.mock.mine(1);
    let again = n.wait_settlement(block.height, SettlementState::Confirmed);
    assert_eq!(again.txid, s.txid);
}

/// Another instance validates imported blocks by the BTC rules (D-034 to D-036): a missing settlement transaction or a
/// tampered payload, or a forced-inclusion native transaction left out, make the block invalid; a new block whose
/// anchor is not on the local BTC chain is rejected (depends on the local view, D-041; when syncing historical blocks
/// it is handled in anchor-invalid mode, see `m6_1_anchor.rs`, D-056); the blocks as they are all pass.
#[test]
fn import_rejects_blocks_that_break_btc_rules() {
    use ksana_consensus::Consensus;
    use ksana_node::chain::Chain;
    use ksana_node::NodeError;

    let ivy = user("m3/ivy");
    let jack = user("m3/jack");
    let n = MockNode::start(&[(&ivy, 1_000)]);
    // One path-B op (no settlement transaction in flight, so this block carries its own) and one native transaction
    // (forced inclusion)
    let op = signed(&ivy, 0, FeeMode::Token { amount: 2 }, jack.account, 3);
    let paid = n.wait_included(&n.submit(&op)).block.unwrap().height;
    let (native, native_op) = native_tx(&n.mock, |input| {
        signed_native(&ivy, 1, input, jack.account, 4)
    });
    n.mock.mine_block(vec![native]).unwrap();
    let forced_at = n
        .wait_included(&native_op.op_id().unwrap())
        .block
        .unwrap()
        .height;
    n.node().set_paused(true);
    std::thread::sleep(Duration::from_millis(300));
    let source = n.node().chain();
    let tip = source.tip().unwrap().header.height;
    let blocks: Vec<ksana_primitives::Block> = (1..=tip)
        .map(|h| {
            let hash = source.store().hash_at(h).unwrap().unwrap();
            source.store().block(&hash).unwrap().unwrap()
        })
        .collect();

    let genesis = {
        let g = source.genesis();
        GenesisFile {
            timestamp: g.timestamp,
            btc_anchor: g.btc_anchor.clone(),
            bits: None,
            block_share_account: None,
            allocations: vec![Allocation {
                account: ivy.account,
                amount: 1_000,
            }],
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let replica = Chain::open(dir.path(), DEVNET, &genesis, n.mock.clone()).unwrap();
    while replica.sync_btc().unwrap() {}
    // Re-solve: the weight follows the (possibly modified) header and the empty-block streak of the selected parent (on
    // the node's linear chain that is parents[0], already in the replica's DAG) (D-090)
    let resolve = |mut b: ksana_primitives::Block| {
        b.header.nonce = 0;
        let sp_streak = replica.streak_of(&b.header.parents[0]).unwrap().unwrap();
        let weight = replica.consensus().weight(&b.header, sp_streak);
        assert!(replica
            .consensus()
            .solve(&mut b.header, weight, 0, 1 << 24)
            .unwrap());
        b
    };
    let invalid = |b: &ksana_primitives::Block, what: &str| match replica.import_block(b, None) {
        Err(NodeError::InvalidBlock(reason)) => eprintln!("{what}: {reason}"),
        other => panic!("{what}: should judge the block invalid: {other:?}"),
    };
    for block in &blocks {
        let h = block.header.height;
        if h == paid {
            // A path-B block may carry no settlement transaction (the body is carried by a later settlement, D-077).
            // When it does carry one, a tampered payload in the reveal transaction's witness makes it invalid (D-081)
            let mut tampered = block.clone();
            let mut pair = SettlementPair::decode(tampered.settlement.as_ref().unwrap()).unwrap();
            let mut elements = pair.reveal.input[0].witness.to_vec();
            let leaf = &mut elements[1];
            let last = leaf.len() - 2; // the last payload byte before OP_ENDIF
            leaf[last] ^= 1;
            pair.reveal.input[0].witness = bitcoin::Witness::from_slice(&elements);
            tampered.settlement = Some(pair.encode());
            invalid(&tampered, "settlement payload tampered");
        }
        if h == forced_at {
            let mut skipped = block.clone();
            skipped
                .entries
                .retain(|e| !matches!(e, Entry::Forced { .. }));
            skipped.header.entries_root = ksana_crypto::entries_root(&skipped.entries).unwrap();
            invalid(
                &resolve(skipped),
                "forced-inclusion native transaction left out",
            );
        }
        let mut off_chain = block.clone();
        off_chain.header.btc_anchor.hash[0] ^= 0xff;
        let off_chain = resolve(off_chain);
        match replica.accept_block(off_chain.clone(), Some(ksana_node::unix_now())) {
            Err(NodeError::Rejected(reason)) => eprintln!("anchor not on the BTC chain: {reason}"),
            other => panic!(
                "a new block whose anchor is not on the BTC chain should be rejected: {other:?}"
            ),
        }
        assert!(!replica.is_invalid(&off_chain.header.hash()).unwrap());
        replica.import_block(block, None).unwrap();
    }
    assert_eq!(replica.tip().unwrap(), source.tip().unwrap());
}
