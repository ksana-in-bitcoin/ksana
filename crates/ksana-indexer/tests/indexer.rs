//! The standalone indexer (SPEC §10.3, D-048): an in-process fast-lane node produces and settles blocks on an in-memory
//! mock BTC chain, and the indexer reads only the same BTC chain and the same genesis config, rebuilding the fast-lane
//! chain and state; a main-chain block is executed only once it has `SETTLEMENT_DEPTH` confirmations on the indexer's
//! main chain (D-063), so with a single node it executes up to `SETTLEMENT_DEPTH` blocks before the node's settled
//! prefix, and the block hash and `state_root` at every height are the same as the node's.
//!
//! - path B (with a settlement transaction), native transactions (forced inclusion), empty blocks, unlock entries;
//! - path A (settlement transaction with the user's input/output pair), and a rescue settlement with the user's
//!   input/output pair removed (§7.3);
//! - more than `MAX_SETTLEMENT_HEADERS` consecutive empty blocks: stops before the gap until the header republish
//!   transaction lands, then catches up (§7.1, D-047);
//! - a BTC reorg makes a settlement's confirmation disappear: rolls back to before that block, catches up again once
//!   the settlement lands again;
//! - the fast-lane main chain switches to a branch with more work: rolls back to the fork point and executes the new
//!   branch;
//! - blocks whose anchor block was reorged out of BTC (D-056): passes over them in anchor-invalid mode (bodies taken
//!   from the settlement payload, native transactions and funding inputs looked up by txid), also when the settlement
//!   transactions confirmed again at the anchor's height, where their payload does not count, and a rescue landed the
//!   body (D-100);
//! - restores the same state after a restart (including a BTC reorg while it was down); when bitcoind is behind the
//!   kept state versions after a restart, waits for it instead of rebuilding from the genesis block;
//! - DAG (D-063): parallel blocks merged by a later block, the indexer executes along the same main chain (the merged
//!   blocks' entries in lenient mode); when a merged block's settlement lands before the header of the main-chain block
//!   at the same height, the indexer does not execute it as a main-chain block;
//! - the mining algorithm is replaceable (D-062): the node and the indexer still agree when both switch to the same
//!   test algorithm; an indexer choosing the algorithm by the network params does not accept these headers;
//! - a block whose `state_root` does not match (valid PoW, with a hand-built settlement transaction) cannot land:
//!   marked, the main chain is re-selected, and it keeps agreeing with the node;
//! - a block with an invalid body (a forced-inclusion native transaction left out, or a path B op with a broken
//!   signature landed by a republish payload with its own confirmations), or a block merging one: marked like a block
//!   that cannot land, the main chain is re-selected and it keeps agreeing with the node; once its data has
//!   `BTC_FINALITY_CONFS` confirmations the finality decision excludes it (D-101), so it never becomes final and
//!   processing goes on;
//! - BTC-anchored finality (D-082): a block whose data has `BTC_FINALITY_CONFS` confirmations is final, a heavier
//!   chain landing only afterwards cannot re-select the main chain, same as the node; a block meeting that condition
//!   but invalid is excluded instead (D-101), on the indexer and the node alike, and the verdicts kept in the index let
//!   the decision go on after a restart without the states discarded since;
//! - a landed block whose `btc_anchor` names no BTC block (the review of D-101), whether its body landed with a wrong
//!   `state_root` or never landed, or a block merging one: excluded before its landing status or verdict is asked
//!   (D-101), so finality goes on along the honest chain, on the indexer and the node alike.

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use bitcoin::secp256k1::{Keypair as BtcKeypair, Secp256k1};
use bitcoin::{Amount, TxOut};
use common::*;
use ksana_btc::client::{BtcClient, BtcError, HeaderInfo, ScannedUtxo, TxInfo, UtxoInfo};
use ksana_btc::headers::{HeaderDag, Insert};
use ksana_btc::mock::MockBtc;
use ksana_btc::settlement::{draft, finalize, producer_script, ProducerUtxo, SettlementPair};
use ksana_btc::tracker::BtcSnapshot;
use ksana_consensus::{Consensus, PowConsensus};
use ksana_indexer::rpc::{IndexerApiClient, StateRootJson, StatusJson};
use ksana_indexer::{Indexer, IndexerConfig};
use ksana_node::chain::Accepted;
use ksana_node::NodeError;
use ksana_primitives::{BlockHeader, BtcAnchor, Entry, SigScheme, BLOCK_VERSION};

#[test]
fn rebuilds_the_settled_prefix_with_the_node_state_roots() {
    let (alice, bob) = (user("t1/alice"), user("t1/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t1");

    // 1: path B (with a settlement transaction); 2, 3: empty blocks, after which block 1 reaches SETTLEMENT_DEPTH and
    // its settlement is broadcast and confirmed
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    let b1 = a.mine();
    assert!(b1.settlement.is_some());
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    // 4: unlocks block 1's sponsored fee and block reward (only an unlock entry, also with a settlement transaction,
    // D-043; the target is block 1's acceptance position, D-079)
    let b4 = a.mine();
    assert_eq!(b1.header.height, 1);
    assert!(
        matches!(
            b4.entries.as_slice(),
            [Entry::Unlock {
                height: 1,
                index: 0
            }]
        ),
        "{:?}",
        b4.entries
    );
    assert!(b4.settlement.is_some());
    // 5: a native transaction is force-included (only a forced-inclusion entry, no settlement transaction)
    let (tx, native) = native_tx(&mock, |input| {
        transfer_native(&alice, 1, input, bob.account, 4)
    });
    mock.mine_block(vec![tx]).unwrap();
    let b5 = a.mine();
    assert!(
        matches!(b5.entries.as_slice(), [Entry::Forced { op, .. }] if *op == native),
        "{:?}",
        b5.entries
    );
    assert!(b5.settlement.is_none());
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    // 8: path B; broadcast and confirmed after 9, 10; 11: unlocks block 8 (its settlement is not broadcast yet: the
    // headers of 9, 10, 11 are not on BTC yet)
    a.submit(transfer(&alice, 2, token(2), bob.account, 5));
    let b8 = a.mine();
    assert_eq!(b8.header.height, 8);
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    let b11 = a.mine();
    assert!(b11
        .entries
        .iter()
        .any(|e| matches!(e, Entry::Unlock { .. })));

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let progress = catch_up(&ix);
    // the main-chain tip is block 8, the one with SETTLEMENT_DEPTH confirmations is block 6 (D-063)
    assert_eq!(progress.height, 6);
    assert_eq!(progress.main_height, 8);
    assert_matches_node(&ix, &a);

    // the node keeps producing blocks and settles block 11: the indexer advances accordingly
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status().height, 9);
    assert_matches_node(&ix, &a);
}

/// The transaction landing a block's data is not the settlement transaction it carries but another one with the same
/// payload and different funding (a rescue settlement, §7.3, D-045): both the node and the indexer count the body
/// carried by the payload (D-077).
#[test]
fn a_rescue_settlement_with_the_same_payload_is_accepted() {
    let (alice, bob) = (user("t2/alice"), user("t2/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t2");

    // first block: the settlement transaction is broadcast and confirmed normally
    a.submit(transfer(&alice, 0, token(2), bob.account, 7));
    let s1 = a.mine();
    assert!(matches!(s1.entries.as_slice(), [Entry::Sponsored { .. }]));
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    // second block: its own settlement transaction is not broadcast; instead a rescue settlement with the same payload,
    // funded by another funding output of the producer, is broadcast
    a.submit(transfer(&alice, 1, token(2), bob.account, 8));
    let s2 = a.mine();
    // s1's settlement is confirmed: s2 first unlocks cumulatively up to s1's acceptance position (block reward, D-074,
    // D-079)
    assert!(matches!(
        s2.entries.as_slice(),
        [Entry::Unlock { height, index: 0 }, Entry::Sponsored { .. }] if *height == s1.header.height
    ));
    a.mine_empty();
    a.mine_empty();
    let original = SettlementPair::decode(s2.settlement.as_ref().unwrap()).unwrap();
    let payload = ksana_btc::payload::attached_payload(&PARAMS, &original).unwrap();
    let point = mock.fund(&a.funding.script, &[100_000])[0];
    let utxo = ProducerUtxo {
        outpoint: point,
        txout: TxOut {
            value: Amount::from_sat(100_000),
            script_pubkey: a.funding.script.clone(),
        },
    };
    let unsigned = draft(
        &PARAMS,
        &[utxo],
        a.funding.script.clone(),
        &a.funding.keypair.x_only_public_key().0,
        &payload,
        2,
    )
    .unwrap();
    let rescue = finalize(&PARAMS, unsigned, &payload, &a.funding.keypair).unwrap();
    assert_ne!(rescue.txid(), original.txid());
    mock.submit(rescue.commit.clone()).unwrap();
    mock.submit(rescue.reveal.clone()).unwrap();
    mock.mine(1);
    a.sync();
    let rescue_txid = rescue.txid();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(
        snapshot
            .settlement_of(&s2.header.hash())
            .map(|(txid, _)| txid),
        Some(rescue_txid),
        "the node counts the rescue settlement as the transaction landing that block's data"
    );

    // the next block with a settlement transaction carries the headers after it; block 2 reaches SETTLEMENT_DEPTH
    // confirmations on the indexer's main chain (D-063)
    a.submit(transfer(&alice, 2, token(2), bob.account, 1));
    let carrier = a.mine();
    assert!(carrier.settlement.is_some());
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert_eq!(
        ix.status().height,
        carrier.header.height - PARAMS.settlement_depth
    );
    assert!(ix.status().height >= s2.header.height);
    assert_matches_node(&ix, &a);
}

#[test]
fn header_gap_waits_for_a_headers_transaction() {
    let (alice, bob) = (user("t3/alice"), user("t3/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t3");
    a.submit(transfer(&alice, 0, token(2), bob.account, 1));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    // more than MAX_SETTLEMENT_HEADERS consecutive empty blocks, then a block with a settlement transaction: it can
    // only carry the most recent MAX_SETTLEMENT_HEADERS − 1 of them
    for _ in 0..PARAMS.max_settlement_headers + 5 {
        a.mine_empty();
    }
    a.submit(transfer(&alice, 1, token(2), bob.account, 1));
    let s = a.mine();
    assert!(s.settlement.is_some());
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    let carried = PARAMS.max_settlement_headers as u64 - 1;
    let first_carried = s.header.height - carried;
    assert!(first_carried > 2);

    // the headers of blocks 2 to first_carried − 1 are not on BTC: the block tree's main chain stops before the gap
    // (the node's settled prefix stops there too), block 1 does not have SETTLEMENT_DEPTH confirmations (D-063)
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let progress = catch_up(&ix);
    assert_eq!(progress.height, 0);
    assert_eq!(
        progress.main_height, 1,
        "the headers after the gap cannot connect to the block tree"
    );
    assert_matches_node(&ix, &a);

    // advances again once the header republish transaction lands, up to SETTLEMENT_DEPTH blocks before the block with
    // the settlement transaction
    let missing: Vec<BlockHeader> = (2..first_carried).map(|h| a.block(h).header).collect();
    mock.mine_block(vec![headers_tx(&mock, missing)]).unwrap();
    let progress = catch_up(&ix);
    assert_eq!(progress.main_height, s.header.height);
    assert_eq!(progress.height, s.header.height - PARAMS.settlement_depth);
    assert_matches_node(&ix, &a);
}

#[test]
fn btc_reorg_rolls_back_and_catches_up_again() {
    let (alice, bob) = (user("t4/alice"), user("t4/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t4");
    a.mine_empty();
    a.mine_empty();
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    // block 6's settlement carries the headers of blocks 4, 5; both settlements are confirmed in the same BTC block
    a.submit(transfer(&alice, 1, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert_eq!(ix.status().height, 4);
    assert_matches_node(&ix, &a);
    let settled = ix.status();

    // BTC reorgs out the block confirming the settlements: the data of blocks 1 to 6 is no longer on BTC, the indexer
    // rolls back to genesis
    mock.reorg(1);
    catch_up(&ix);
    assert_eq!(ix.status().height, 0);
    assert_eq!(ix.block_at(1), None);
    assert_matches_node(&ix, &a);

    // the settlement transactions return to the mempool; once they are mined again the indexer catches up again, with
    // the same status as before
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status(), settled);
    assert_matches_node(&ix, &a);
}

/// After the anchor block is reorged out of BTC: BTC block X, anchored by blocks 1, 2, is reorged out, and the native
/// transaction force-included by block 1 is confirmed again at a different height on the new branch. The node retreats
/// below block 1 (D-104): alice's two ops go back into the entry pool, and the next block, anchored on the new branch,
/// force-includes the native transaction again at its new height and includes the two ops again. The indexer and a
/// fresh node take the same main chain, agreeing with the node height by height.
#[test]
fn blocks_whose_anchor_was_reorged_out_leave_the_main_chain() {
    let (alice, bob, carol) = (user("t9/alice"), user("t9/bob"), user("t9/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, "t9");

    // the native transaction is confirmed in BTC block Y, the next block X is empty; blocks 1, 2 anchor X
    let (tx, native) = native_tx(&mock, |input| {
        transfer_native(&carol, 0, input, bob.account, 4)
    });
    mock.mine_block(vec![tx]).unwrap();
    mock.mine_block(Vec::new()).unwrap();
    a.sync();
    a.submit(transfer(&alice, 0, token(2), bob.account, 7));
    let b1 = a.mine();
    assert!(
        matches!(b1.entries.as_slice(), [Entry::Forced { op, .. }, Entry::Sponsored { .. }] if *op == native),
        "{:?}",
        b1.entries
    );
    a.submit(transfer(&alice, 1, token(3), bob.account, 10));
    let b2 = a.mine();
    let x = b1.header.btc_anchor.clone();
    assert_eq!(b2.header.btc_anchor, x);

    // BTC reorgs out X and Y; the new branch first mines an empty block, then the native transaction is confirmed again
    // on the new branch (at a different height)
    mock.reorg(2);
    mock.mine_block(Vec::new()).unwrap();
    mock.mine(2);
    assert!(!mock.header(&x.hash).unwrap().unwrap().in_best_chain);

    // the node retreats to the genesis block, and the next block includes everything again
    a.sync();
    assert_eq!(a.tip_height(), 0);
    let again = a.mine();
    assert_ne!(again.header.btc_anchor, x);
    assert!(
        matches!(again.entries.as_slice(), [Entry::Forced { op, .. }, Entry::Sponsored { .. }, Entry::Sponsored { .. }] if *op == native),
        "{:?}",
        again.entries
    );
    settle_two_more(&a, &alice, &bob, 2);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert!(ix.status().height >= 1);
    assert_matches_node(&ix, &a);
    import_history(&Side::new(&mock, &genesis, "t9-fresh"), &a);
}

/// The case D-056 exists for, with the forced-inclusion transaction confirmed again above the reorged-out anchor (the
/// new branch has three blocks without it first): the node retreats below blocks 1, 2 (D-104), and the next block
/// force-includes the transaction again at its new height. A fresh node given the old blocks takes them as side blocks
/// in anchor-invalid mode: a forced-inclusion entry of an anchor-invalid block is looked up from the block's window
/// start up to the window end of a block anchored now, not only up to its own window end (D-100), so they are not
/// refused. The indexer and the fresh node agree with the node height by height.
#[test]
fn a_forced_transaction_confirmed_again_above_the_old_anchor_is_included_again() {
    let (alice, bob, carol) = (user("t10/alice"), user("t10/bob"), user("t10/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, "t10");

    // the native transaction is confirmed in BTC block Y, the next block X is empty; blocks 1, 2 anchor X
    let (tx, native) = native_tx(&mock, |input| {
        transfer_native(&carol, 0, input, bob.account, 4)
    });
    let txid = tx.compute_txid();
    mock.mine_block(vec![tx]).unwrap();
    mock.mine_block(Vec::new()).unwrap();
    a.sync();
    a.submit(transfer(&alice, 0, token(2), bob.account, 7));
    let b1 = a.mine();
    assert!(
        matches!(b1.entries.as_slice(), [Entry::Forced { op, .. }, Entry::Sponsored { .. }] if *op == native),
        "{:?}",
        b1.entries
    );
    a.submit(transfer(&alice, 1, token(3), bob.account, 10));
    let b2 = a.mine();
    let x = b1.header.btc_anchor.clone();
    assert_eq!(b2.header.btc_anchor, x);

    // BTC reorgs out X and Y; the new branch mines three empty blocks, then the native transaction confirms again two
    // heights above X
    mock.reorg(2);
    for _ in 0..3 {
        mock.mine_block(Vec::new()).unwrap();
    }
    mock.mine(1);
    assert!(!mock.header(&x.hash).unwrap().unwrap().in_best_chain);
    let confirmed = mock
        .transaction(&<bitcoin::Txid as bitcoin::hashes::Hash>::to_byte_array(
            txid,
        ))
        .unwrap()
        .and_then(|info| info.block)
        .and_then(|hash| mock.header(&hash).unwrap())
        .unwrap();
    assert_eq!(confirmed.height, x.height + 2);

    // the node retreats to the genesis block; the next block force-includes the transaction at its new height
    a.sync();
    assert_eq!(a.tip_height(), 0);
    let again = a.mine();
    assert!(
        matches!(again.entries.as_slice(), [Entry::Forced { op, .. }, Entry::Sponsored { .. }, Entry::Sponsored { .. }] if *op == native),
        "{:?}",
        again.entries
    );
    settle_two_more(&a, &alice, &bob, 2);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert!(ix.status().height >= 1);
    assert_matches_node(&ix, &a);

    let fresh = Side::new(&mock, &genesis, "t10-fresh");
    for block in [&b1, &b2] {
        assert_eq!(
            fresh.chain.accept_block(block, None).unwrap().outcome,
            Accepted::Side
        );
    }
    import_history(&fresh, &a);
}

/// The settlement transactions of a block anchored at BTC block X are broadcast while X is the tip, and BTC reorgs X out
/// before they confirm: they confirm again in X's replacement, at X's height, where their payload does not count (D-100).
/// The reorg took the block's anchor off the BTC best chain, so the block leaves the main chain (D-104) and its op is
/// included again by a block anchored on the new branch, whose settlement lands. The indexer agrees with the node height
/// by height; a fresh node rejects the old block as a new block (the anchor is not on the local BTC best chain, not
/// marked invalid), takes it as a side block when syncing, and takes the same main chain.
#[test]
fn a_settlement_confirmed_again_at_its_anchor_height_leaves_its_block_behind() {
    let (alice, bob) = (user("t11/alice"), user("t11/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t11");

    // block 1 anchors X, the BTC tip; its settlement transactions are broadcast while X is the tip
    mock.mine(1);
    a.sync();
    let op = transfer(&alice, 0, token(2), bob.account, 5);
    a.submit(op.clone());
    let b1 = a.mine();
    let x = b1.header.btc_anchor.clone();
    assert_eq!(x.height, mock.tip_height());
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    let pair = SettlementPair::decode(b1.settlement.as_ref().unwrap()).unwrap();
    assert!(mock.mempool().iter().any(|tx| {
        <bitcoin::Txid as bitcoin::hashes::Hash>::to_byte_array(tx.compute_txid()) == pair.txid()
    }));

    // BTC reorgs X out; X's replacement confirms the settlement transactions at X's height, where their payload does
    // not count, and the new branch grows by one more block
    mock.reorg(1);
    mock.mine(2);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(!snapshot.contains(&x));
    assert_eq!(snapshot.ksn1_height(&pair.txid()), None);

    // the node retreats to the genesis block; the next block includes the op again and its settlement lands
    assert_eq!(a.tip_height(), 0);
    let again = a.mine();
    assert!(again.header.btc_anchor.height > x.height);
    assert!(
        matches!(again.entries.as_slice(), [Entry::Sponsored { op: included }] if *included == op),
        "{:?}",
        again.entries
    );
    settle_two_more(&a, &alice, &bob, 1);
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(a
        .chain
        .carried_on_chain(&again, &snapshot)
        .unwrap()
        .is_some());

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert!(ix.status().height >= again.header.height);
    assert_matches_node(&ix, &a);

    let fresh = Side::new(&mock, &genesis, "t11-fresh");
    fresh.sync();
    match fresh
        .chain
        .accept_block(b1.clone(), Some(b1.header.timestamp))
    {
        Err(NodeError::Rejected(m)) => assert!(m.contains("best chain"), "{m}"),
        other => panic!("should be rejected as a new block: {other:?}"),
    }
    assert!(!fresh.chain.is_invalid(&b1.header.hash()).unwrap());
    assert_eq!(
        fresh.chain.accept_block(&b1, None).unwrap().outcome,
        Accepted::Side
    );
    import_history(&fresh, &a);
}

/// Lets the node's last block with a body land, then produces one more block with alice's op of nonce `nonce` and two
/// empty blocks and lands its settlement too, so that the earlier blocks reach `SETTLEMENT_DEPTH` confirmations on the
/// indexer's main chain (D-063).
fn settle_two_more(a: &Side, alice: &User, bob: &User, nonce: u64) {
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    a.mock.mine(1);
    a.submit(transfer(alice, nonce, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    a.mock.mine(1);
    a.sync();
}

/// A fresh node takes `a`'s main chain from the genesis block as historical blocks: every block extends its main chain
/// with the same state root, none of them being anchored off the BTC best chain (D-104).
fn import_history(fresh: &Side, a: &Side) {
    for height in 1..=a.tip_height() {
        let block = a.block(height);
        let on_chain = a
            .mock
            .header(&block.header.btc_anchor.hash)
            .unwrap()
            .is_some_and(|info| info.in_best_chain);
        assert!(on_chain, "height {height}");
        let outcome = fresh.chain.accept_block(&block, None).unwrap().outcome;
        assert!(
            matches!(outcome, Accepted::Extended(_)),
            "height {height}: {outcome:?}"
        );
        assert_eq!(
            fresh.chain.tip().unwrap().header.state_root,
            block.header.state_root,
            "height {height}"
        );
    }
    assert_eq!(fresh.chain.tip().unwrap().hash, a.chain.tip().unwrap().hash);
}

/// When the rollback target is beyond the rollback range (old state already pruned), the state is rebuilt from the
/// genesis block. `REORG_WINDOW` only affects local pruning, not the execution result, so here only the indexer's is
/// set to 2.
#[test]
fn rollback_beyond_the_reorg_window_rebuilds_from_genesis() {
    let (alice, bob) = (user("t8/alice"), user("t8/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t8");
    // three blocks with settlement transactions, whose settlements are confirmed in the same BTC block (the fast-lane
    // blocks all anchor before it)
    for nonce in 0..3 {
        a.submit(transfer(&alice, nonce, token(3), bob.account, 10));
        a.mine();
        a.mine_empty();
        a.mine_empty();
    }
    a.broadcast_settlements();
    mock.mine(1);

    let mut params = PARAMS.clone();
    params.reorg_window = 2;
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer_with(dir.path(), &genesis, &mock, params);
    catch_up(&ix);
    // the main-chain tip is block 7, the one with SETTLEMENT_DEPTH confirmations is block 5 (D-063)
    assert_eq!(ix.status().height, 5);
    assert_matches_node(&ix, &a);
    let settled = ix.status();

    // all three settlements are reorged out: the rollback target is the genesis block, but the state only keeps the
    // versions from height 3 on
    mock.reorg(1);
    catch_up(&ix);
    assert_eq!(ix.status().height, 0);
    assert_eq!(
        ix.block_at(0),
        Some((genesis_hash(&a), a.block(0).header.state_root))
    );
    assert_matches_node(&ix, &a);
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status(), settled);
    assert_matches_node(&ix, &a);
}

fn genesis_hash(side: &Side) -> [u8; 32] {
    side.chain.genesis().hash()
}

/// bitcoind that is behind (reindexing, or catching up after downtime): shows the mock chain only up to height
/// `visible`.
struct Lagging {
    mock: Arc<MockBtc>,
    visible: AtomicU32,
}

impl Lagging {
    fn visible(&self) -> u32 {
        self.visible.load(Ordering::Relaxed)
    }

    /// Whether the block is known and at or below the visible height.
    fn shows(&self, hash: &[u8; 32]) -> Result<bool, BtcError> {
        Ok(self
            .mock
            .header(hash)?
            .is_some_and(|info| info.height <= self.visible()))
    }
}

impl BtcClient for Lagging {
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
        let (height, hash) = self.mock.tip()?;
        if height <= self.visible() {
            return Ok((height, hash));
        }
        let hash = self
            .mock
            .block_hash(self.visible())?
            .ok_or_else(|| BtcError::Rpc("no block at the visible height".into()))?;
        Ok((self.visible(), hash))
    }

    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
        if height > self.visible() {
            return Ok(None);
        }
        self.mock.block_hash(height)
    }

    fn block(&self, hash: &[u8; 32]) -> Result<Option<bitcoin::Block>, BtcError> {
        if !self.shows(hash)? {
            return Ok(None);
        }
        self.mock.block(hash)
    }

    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
        Ok(self
            .mock
            .header(hash)?
            .filter(|info| info.height <= self.visible()))
    }

    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
        let Some(info) = self.mock.transaction(txid)? else {
            return Ok(None);
        };
        match info.block {
            Some(block) if !self.shows(&block)? => Ok(None),
            _ => Ok(Some(info)),
        }
    }

    fn utxo(
        &self,
        outpoint: &bitcoin::OutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError> {
        self.mock.utxo(outpoint, include_mempool)
    }

    fn scan_utxos(&self, script: &bitcoin::Script) -> Result<Vec<ScannedUtxo>, BtcError> {
        self.mock.scan_utxos(script)
    }

    fn test_accept(&self, txs: &[&bitcoin::Transaction]) -> Result<Result<(), String>, BtcError> {
        self.mock.test_accept(txs)
    }

    fn broadcast(&self, tx: &bitcoin::Transaction) -> Result<Result<(), String>, BtcError> {
        self.mock.broadcast(tx)
    }

    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
        self.mock.in_mempool(txid)
    }

    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
        self.mock.mempool_txids()
    }

    fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
        Ok(self
            .mock
            .txindex_height()?
            .map(|height| height.min(self.visible())))
    }
}

/// The (oldest, latest) versions of the indexer's state store in `dir` (the indexer must be closed).
fn state_versions(dir: &std::path::Path) -> (u64, u64) {
    let state = ksana_state::StateDb::open(&dir.join("state")).unwrap();
    (
        state.oldest_version().unwrap(),
        state.latest_version().unwrap(),
    )
}

/// After a restart the state store is verified by state root against the main chain on BTC (D-048). When bitcoind is
/// behind the chain the indexer verified before (reindexing, or catching up after downtime), the processable prefix
/// can lie below the oldest state version kept (`REORG_WINDOW`, set to 2 for the indexer here, below the final tip),
/// where nothing can be checked yet: the indexer waits for bitcoind instead of rebuilding from the genesis block, and
/// once bitcoind has caught up it resumes with the same status without re-executing anything (the verdicts the
/// finality decision asks for come from the index, D-101).
#[test]
fn restart_with_bitcoind_behind_waits_instead_of_rebuilding() {
    let (alice, bob) = (user("t20/alice"), user("t20/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t20");
    // three blocks with settlement transactions, each settled in its own BTC block, then BTC-final: the states below
    // the final tip may be discarded (D-101)
    let mut settled_at = Vec::new();
    for nonce in 0..3 {
        a.submit(transfer(&alice, nonce, token(3), bob.account, 10));
        a.mine();
        a.mine_empty();
        a.mine_empty();
        a.broadcast_settlements();
        mock.mine(1);
        settled_at.push(mock.tip_height());
    }
    mock.mine(PARAMS.btc_finality_confs);
    let mut params = PARAMS.clone();
    params.reorg_window = 2;
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer_with(dir.path(), &genesis, &mock, params.clone());
    catch_up(&ix);
    // the main-chain tip is block 7, which is final (blocks 8, 9 have not landed); versions 5 to 7 are kept
    assert_eq!(ix.status().height, 7);
    assert_eq!(ix.status().finalized_height, 7);
    assert_matches_node(&ix, &a);
    let before = ix.status();
    drop(ix);
    assert_eq!(state_versions(dir.path()), (5, 7));

    // restart while bitcoind has only reached the second settlement: the main chain on BTC ends at block 4, the
    // processable prefix at block 2, below the oldest version kept
    let lagging = Arc::new(Lagging {
        mock: mock.clone(),
        visible: AtomicU32::new(settled_at[1]),
    });
    let open = || {
        Indexer::open(IndexerConfig {
            data_dir: dir.path().to_path_buf(),
            genesis: genesis.clone(),
            params: params.clone(),
            btc: lagging.clone(),
            pow_algorithm: None,
            pruning: ksana_btc::prune::Pruning::default(),
        })
        .unwrap()
    };
    let ix = open();
    let progress = catch_up(&ix);
    assert_eq!(
        (progress.main_height, progress.btc_height),
        (4, Some(settled_at[1]))
    );
    // waiting: nothing is verified yet, the status still comes from the metadata, the state store is untouched
    let status = ix.status();
    assert_eq!(
        (status.height, status.hash, status.state_root),
        (before.height, before.hash, before.state_root),
        "the indexer should wait for bitcoind rather than rebuild from the genesis block"
    );
    assert_eq!(ix.block_at(0), None);
    drop(ix);
    assert_eq!(state_versions(dir.path()), (5, 7));

    // bitcoind catches up: the state store is verified by state root and the indexer resumes with the same status; the
    // finality decision takes the verdicts on blocks 1 to 7 from the index (their parents' states are gone, D-101)
    let ix = open();
    catch_up(&ix);
    lagging.visible.store(u32::MAX, Ordering::Relaxed);
    catch_up(&ix);
    assert_eq!(ix.status(), before);
    assert_matches_node(&ix, &a);
    drop(ix);
    assert_eq!(state_versions(dir.path()), (5, 7));
}

#[test]
fn main_chain_switch_rolls_back_to_the_fork() {
    let (alice, bob, carol) = (user("t5/alice"), user("t5/bob"), user("t5/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t5a");
    let b = Side::new(&mock, &genesis, "t5b");

    // A's branch: the settlements of blocks 1, 4 land first (block 4's settlement carries the headers of blocks 2, 3)
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.submit(transfer(&alice, 1, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert_eq!(ix.status().height, 2);
    assert_matches_node(&ix, &a);

    // B's branch (the same nonce spent on another transfer) is longer and has more work once its settlement lands: the
    // indexer rolls back to genesis and executes B's branch
    for _ in 0..5 {
        b.mine_empty();
    }
    b.submit(transfer(&alice, 0, token(4), carol.account, 20));
    b.mine();
    b.mine_empty();
    b.mine_empty();
    b.broadcast_settlements();
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status().height, 4);
    assert_matches_node(&ix, &b);
    assert_ne!(
        ix.block_at(1).map(|(hash, _)| hash),
        Some(a.block(1).header.hash())
    );
}

#[test]
fn restart_resumes_with_the_same_status() {
    let (alice, bob) = (user("t6/alice"), user("t6/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t6");
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.submit(transfer(&alice, 1, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert_eq!(ix.status().height, 2);
    assert_matches_node(&ix, &a);
    let before = ix.status();

    // restart: before rescanning BTC the status comes from the metadata, after the scan it is exactly the same as
    // before
    drop(ix);
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let reopened = ix.status();
    assert_eq!(
        (reopened.height, reopened.hash, reopened.state_root),
        (before.height, before.hash, before.state_root)
    );
    assert_eq!(reopened.btc_height, None);
    catch_up(&ix);
    assert_eq!(ix.status(), before);
    assert_matches_node(&ix, &a);

    // while it is down BTC reorgs out the confirmation of block 4's settlement (the headers of blocks 2 to 4 landed
    // with it): after the restart the state roots are checked against the main chain, only block 1 is left on the main
    // chain without SETTLEMENT_DEPTH confirmations, roll back to the genesis block (D-063); catches up again once the
    // settlement lands again
    drop(ix);
    mock.reorg(1);
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert_eq!(ix.status().height, 0);
    assert_matches_node(&ix, &a);
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status(), before);
    assert_matches_node(&ix, &a);

    // the node keeps advancing, the indexer advances accordingly
    a.submit(transfer(&alice, 2, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status().height, 5);
    assert_matches_node(&ix, &a);

    // JSON-RPC: hex and decimal strings (the same conventions as the node RPC)
    let status = ix.status();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (addr, handle) = rt
        .block_on(ksana_indexer::rpc::start_server(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(ix),
        ))
        .unwrap();
    let client = jsonrpsee::http_client::HttpClientBuilder::default()
        .build(format!("http://{addr}"))
        .unwrap();
    let json: StatusJson = rt.block_on(client.status()).unwrap();
    assert_eq!(json, StatusJson::from(status.clone()));
    let value = serde_json::to_value(&json).unwrap();
    assert_eq!(value["height"], "5");
    assert_eq!(value["hash"], hex::encode(status.hash));
    assert_eq!(value["btc_height"], mock.tip_height());
    assert!(value["halted"].is_null());
    let root: StateRootJson = rt.block_on(client.state_root("2".into())).unwrap();
    let block = a.block(2);
    assert_eq!(
        (root.height, root.hash, root.state_root),
        (2, block.header.hash(), block.header.state_root)
    );
    let by_hash: StateRootJson = rt
        .block_on(client.state_root(hex::encode(block.header.hash())))
        .unwrap();
    assert_eq!(by_hash, root);
    let latest: StateRootJson = rt.block_on(client.state_root("latest".into())).unwrap();
    assert_eq!((latest.height, latest.hash), (5, status.hash));
    match rt.block_on(client.state_root("6".into())) {
        Err(jsonrpsee::core::ClientError::Call(e)) => assert_eq!(e.code(), -32004),
        other => panic!("an unexecuted height should return -32004: {other:?}"),
    }
    match rt.block_on(client.state_root("x".into())) {
        Err(jsonrpsee::core::ClientError::Call(e)) => assert_eq!(e.code(), -32602),
        other => panic!("a malformed block ID should return -32602: {other:?}"),
    }
    // The node's limits (D-132): a batch of more than MAX_BATCH_LEN calls is refused as a whole
    let batch = |len: u32| {
        let calls: Vec<String> = (0..len)
            .map(|id| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"indexer_status"}}"#))
            .collect();
        post(addr, &format!("[{}]", calls.join(",")))
    };
    let answers = batch(ksana_rpc::MAX_BATCH_LEN);
    assert_eq!(
        answers.as_array().map(Vec::len),
        Some(ksana_rpc::MAX_BATCH_LEN as usize)
    );
    let refused = batch(ksana_rpc::MAX_BATCH_LEN + 1);
    assert_eq!(refused["error"]["code"], -32010, "{refused}");
    handle.stop().unwrap();
    rt.block_on(handle.stopped());
}

/// One raw HTTP POST of a JSON-RPC body to `addr`; the answer's JSON.
fn post(addr: std::net::SocketAddr, body: &str) -> serde_json::Value {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (_, body) = response.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap()
}

/// The DAA score of block `hash` in the node's DAG.
fn daa_score_of(a: &Side, hash: &[u8; 32]) -> u64 {
    a.chain.ghostdag(hash).unwrap().unwrap().daa_score
}

/// The `bits` of a block following `parent` (DAA score `parent_daa_score`) with no merge set: independent of this
/// block's weight (D-084).
fn next_bits(a: &Side, parent: &BlockHeader, parent_daa_score: u64) -> u32 {
    a.chain
        .consensus()
        .next_bits(parent, parent_daa_score + 1, 1)
        .unwrap()
}

/// The anchor of a block produced now: the BTC tip.
fn btc_tip_anchor(mock: &MockBtc) -> BtcAnchor {
    let (height, hash) = mock.tip().unwrap();
    BtcAnchor { height, hash }
}

/// The header of a forged block following the node's block `parent`, with the given anchor, entries and `state_root`,
/// valid PoW.
fn forge_header_on(
    a: &Side,
    parent: &BlockHeader,
    btc_anchor: BtcAnchor,
    entries: &[Entry],
    state_root: [u8; 32],
    seed: &str,
) -> BlockHeader {
    let parent_hash = parent.hash();
    let mut header = BlockHeader {
        version: BLOCK_VERSION,
        height: parent.height + 1,
        parents: vec![parent_hash],
        btc_anchor,
        entries_root: ksana_crypto::entries_root(entries).unwrap(),
        state_root,
        producer: user(seed).account,
        timestamp: parent.timestamp + 1,
        bits: next_bits(a, parent, daa_score_of(a, &parent_hash)),
        nonce: 0,
    };
    // the weight follows the parent's empty-block streak (D-090)
    let streak = a.chain.streak_of(&parent_hash).unwrap().unwrap();
    let weight = a.chain.consensus().weight(&header, streak);
    assert!(a
        .chain
        .consensus()
        .solve(&mut header, weight, 0, 1 << 24)
        .unwrap());
    header
}

/// The header of a forged block following the node's tip, with the given entries and `state_root`, anchored at the BTC
/// tip, valid PoW.
fn forge_header(
    a: &Side,
    mock: &Arc<MockBtc>,
    entries: &[Entry],
    state_root: [u8; 32],
    seed: &str,
) -> BlockHeader {
    let tip = a.chain.tip().unwrap();
    forge_header_on(
        a,
        &tip.header,
        btc_tip_anchor(mock),
        entries,
        state_root,
        seed,
    )
}

/// Forges a block: following the node's tip, with the given entries and `state_root`, valid PoW; the settlement
/// transaction is funded by the forger, and its payload carries the not-yet-landed headers the node would carry when
/// producing a block too, followed by this block's header and entries (D-063). The settlement transaction is mined.
fn forge(
    a: &Side,
    mock: &Arc<MockBtc>,
    entries: Vec<Entry>,
    state_root: [u8; 32],
    seed: &str,
) -> BlockHeader {
    let header = forge_header(a, mock, &entries, state_root, seed);
    settle_forged(a, mock, &header, &entries, seed);
    header
}

/// Lands a forged block with its body: a settlement transaction pair funded by the forger, whose payload carries the
/// not-yet-landed headers the node would carry when producing a block on the same parents too, followed by the block's
/// header and entries (D-063). The pair is mined.
fn settle_forged(
    a: &Side,
    mock: &Arc<MockBtc>,
    header: &BlockHeader,
    entries: &[Entry],
    seed: &str,
) {
    let snapshot = a.chain.btc().snapshot().unwrap();
    let carry = a.chain.payload_carry(&header.parents, &snapshot).unwrap();
    let mut headers = carry.headers.clone();
    headers.push(header.clone());
    let payload =
        ksana_btc::payload::payload(&PARAMS, headers, entries, carry.payload_bodies().unwrap())
            .unwrap();
    let key = BtcKeypair::from_secret_key(&Secp256k1::new(), &btc_secret(seed));
    let script = producer_script(&key.x_only_public_key().0);
    let point = mock.fund(&script, &[100_000])[0];
    let utxo = ProducerUtxo {
        outpoint: point,
        txout: TxOut {
            value: Amount::from_sat(100_000),
            script_pubkey: script.clone(),
        },
    };
    let unsigned = draft(
        &PARAMS,
        &[utxo],
        script,
        &key.x_only_public_key().0,
        &payload,
        2,
    )
    .unwrap();
    let pair = finalize(&PARAMS, unsigned, &payload, &key).unwrap();
    mock.submit(pair.commit).unwrap();
    mock.submit(pair.reveal).unwrap();
    mock.mine(1);
}

/// The headers of `SETTLEMENT_DEPTH` empty blocks following `parent` (valid PoW; their selected chain passes through
/// `parent`, the `state_root` does not matter), landed with a header republish transaction: `parent` reaches
/// `SETTLEMENT_DEPTH` confirmations on the indexer's main chain and will be executed (D-063).
fn confirm_forged(a: &Side, mock: &Arc<MockBtc>, parent: &BlockHeader, seed: &str) {
    let headers = forged_confirmations(a, parent, seed);
    mock.mine_block(vec![headers_tx(mock, headers)]).unwrap();
}

/// The headers of `SETTLEMENT_DEPTH` empty blocks following the forged block `parent` (whose selected parent is a node
/// block), valid PoW, not landed.
fn forged_confirmations(a: &Side, parent: &BlockHeader, seed: &str) -> Vec<BlockHeader> {
    let consensus = a.chain.consensus();
    // the forged block is not in the node's DAG: its DAA score is the selected parent's (a node block) score plus its
    // weight (D-084); the weight and the empty-block streak follow the selected parent (D-090)
    let sp_streak = a.chain.streak_of(&parent.parents[0]).unwrap().unwrap();
    let mut streak = consensus.streak(parent, sp_streak);
    let mut daa_score = daa_score_of(a, &parent.parents[0]) + consensus.weight(parent, sp_streak);
    let mut headers = Vec::new();
    let mut parent = parent.clone();
    for _ in 0..PARAMS.settlement_depth {
        let mut header = BlockHeader {
            version: BLOCK_VERSION,
            height: parent.height + 1,
            parents: vec![parent.hash()],
            btc_anchor: parent.btc_anchor.clone(),
            entries_root: ksana_crypto::entries_root(&[]).unwrap(),
            state_root: parent.state_root,
            producer: user(seed).account,
            timestamp: parent.timestamp + 1,
            bits: next_bits(a, &parent, daa_score),
            nonce: 0,
        };
        let weight = consensus.weight(&header, streak);
        assert!(consensus.solve(&mut header, weight, 0, 1 << 24).unwrap());
        headers.push(header.clone());
        daa_score += weight;
        streak = consensus.streak(&header, streak);
        parent = header;
    }
    headers
}

/// A block with valid PoW and a valid settlement transaction but a wrong `state_root` (D-063): executed as a main-chain
/// block, the check fails, so it cannot land; the indexer marks it, re-selects the main chain (back to the node's chain
/// here) and does not halt, agreeing with the node.
#[test]
fn a_block_with_a_wrong_state_root_is_disqualified() {
    let (alice, bob) = (user("t7/alice"), user("t7/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t7");
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();

    // a forged block following the node's tip (block 3): the entry is a valid path B op, the state_root is wrong
    let entries = vec![Entry::Sponsored {
        op: transfer(&alice, 1, token(2), bob.account, 1),
    }];
    let forged = forge(&a, &mock, entries, [0x55; 32], "t7/forger");
    confirm_forged(&a, &mock, &forged, "t7/forger");

    // the forged block reaches SETTLEMENT_DEPTH confirmations and fails the check once executed: marked, then the
    // node's chain is re-selected (tip block 3, the one with enough confirmations is block 1)
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let progress = catch_up(&ix);
    let status = ix.status();
    assert_eq!(status.halted, None);
    assert_eq!(progress.main_height, 3);
    assert_eq!(status.height, 1);
    assert_eq!(ix.find_block(&forged.hash()), None);
    assert_matches_node(&ix, &a);
    // the node keeps producing blocks (it does not know the forged block): the indexer advances along the node's chain
    a.submit(transfer(&alice, 1, token(2), bob.account, 2));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status().height, 2);
    assert_matches_node(&ix, &a);
}

/// A block with an invalid body (a native transaction within the anchored range left out, §7.4): honest nodes do not
/// accept it, yet its header and entries landed on BTC through a settlement. When the indexer reaches it, it marks it
/// like a block that cannot land and re-selects the main chain (D-063), agreeing with the node. Once its data has
/// `BTC_FINALITY_CONFS` confirmations, the finality decision judges it on top of the final tip and excludes it with
/// the blocks built on it (D-101): the final tip stops before it on the indexer, on a freshly opened indexer and on the
/// node, which never had the block and judges it from the data on BTC; processing does not halt.
#[test]
fn a_block_missing_a_forced_entry_is_disqualified_and_never_final() {
    let (alice, bob, carol) = (user("t11/alice"), user("t11/bob"), user("t11/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, "t11");
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();

    // after the native transaction lands, the forged block anchors after it but does not include it
    let (tx, _) = native_tx(&mock, |input| {
        transfer_native(&carol, 0, input, bob.account, 4)
    });
    mock.mine_block(vec![tx]).unwrap();
    a.sync();
    let entries = vec![Entry::Sponsored {
        op: transfer(&alice, 1, token(2), bob.account, 1),
    }];
    let forged = forge(&a, &mock, entries, [0x66; 32], "t11/forger");
    confirm_forged(&a, &mock, &forged, "t11/forger");

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let progress = catch_up(&ix);
    let status = ix.status();
    assert_eq!(status.halted, None);
    // back on the node's chain: tip block 3, the block with SETTLEMENT_DEPTH confirmations is block 1
    assert_eq!(progress.main_height, 3);
    assert_eq!(status.height, 1);
    assert_eq!(ix.find_block(&forged.hash()), None);
    assert_matches_node(&ix, &a);

    // the forged block's data gets BTC_FINALITY_CONFS confirmations: blocks 1 to 3 are final, the forged block is
    // excluded, nothing halts
    mock.mine(PARAMS.btc_finality_confs);
    catch_up(&ix);
    let block3 = a.block(3).header.hash();
    let status = ix.status();
    assert_eq!(status.halted, None);
    assert_eq!(
        (status.finalized_height, status.finalized_hash),
        (3, block3)
    );
    assert_eq!(status.height, 3);
    assert_eq!(ix.find_block(&forged.hash()), None);
    assert_matches_node(&ix, &a);
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (block3, 3));

    // a freshly opened indexer: blocks 1 to 3 are final and checked as usual; the forged block is judged when the
    // decision reaches it and excluded
    let dir = tempfile::tempdir().unwrap();
    let fresh = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&fresh);
    let status = fresh.status();
    assert_eq!(status.halted, None);
    assert_eq!(
        (status.finalized_height, status.finalized_hash),
        (3, block3)
    );
    assert_eq!(status.height, 3);
    assert_eq!(fresh.find_block(&forged.hash()), None);
    for height in 0..=3 {
        let block = a.block(height);
        assert_eq!(
            fresh.block_at(height),
            Some((block.header.hash(), block.header.state_root))
        );
    }
    // the node keeps producing blocks on the final tip: both indexers follow
    a.submit(transfer(&alice, 1, token(2), bob.account, 2));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    assert_eq!(a.chain.finalized().unwrap().1, 4);
    for ix in [&ix, &fresh] {
        catch_up(ix);
        assert_eq!(ix.status().halted, None);
        assert_eq!(ix.status().height, 4);
        assert_matches_node(ix, &a);
    }
}

/// DAG (D-063): two nodes produce parallel blocks (the same op in both blocks, another op in only one of them) and
/// exchange them; a later block merges the other parallel block. The indexer rebuilds the DAG from BTC data alone and
/// executes along the same main chain (the merged block's entries in lenient mode); the block hash and `state_root` at
/// every height within the settled closure are the same as the node's.
#[test]
fn parallel_blocks_merged_by_the_node_are_indexed() {
    let (alice, bob, carol, dave) = (
        user("t10/alice"),
        user("t10/bob"),
        user("t10/carol"),
        user("t10/dave"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, "t10a");
    let b = Side::new(&mock, &genesis, "t10b");
    let duplicate = transfer(&alice, 0, token(3), bob.account, 10);
    a.submit(duplicate.clone());
    b.submit(duplicate);
    b.submit(transfer(&carol, 0, token(4), dave.account, 1));
    let a1 = a.mine();
    let b1 = b.mine();
    assert_eq!(a1.header.parents, b1.header.parents);
    a.accept(&b1);
    b.accept(&a1);
    // A's next block merges the other parallel block
    let merging = a.mine();
    assert_eq!(merging.header.parents.len(), 2);
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    // the headers of the merging block and the empty blocks after it are carried by the next block with a settlement
    // transaction
    a.submit(transfer(&alice, 1, token(2), bob.account, 1));
    let carrier = a.mine();
    assert!(carrier.settlement.is_some());
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert_eq!(
        ix.status().height,
        carrier.header.height - PARAMS.settlement_depth
    );
    assert!(ix.status().height >= merging.header.height);
    assert_matches_node(&ix, &a);
    // the merged parallel block is not on the main chain, but its execution result is in the state root
    let other = if a.block(1).header.hash() == a1.header.hash() {
        &b1
    } else {
        &a1
    };
    assert_eq!(ix.find_block(&other.header.hash()), None);
}

/// A test mining algorithm (D-062): PoW value = a single SHA-256 of the header encoding, interpreted big-endian.
#[derive(Debug)]
struct Sha256Be;

impl ksana_consensus::PowAlgorithm for Sha256Be {
    fn name(&self) -> &'static str {
        "sha256-be"
    }

    fn pow_value(
        &self,
        header: &BlockHeader,
    ) -> Result<ksana_consensus::U256, ksana_consensus::ConsensusError> {
        let mut digest = ksana_primitives::hash::sha256(&header.to_bytes());
        digest.reverse();
        Ok(ksana_consensus::U256::from_le_bytes(digest))
    }
}

/// The mining algorithm is replaceable (D-062): the node produces and settles blocks with the test algorithm (genesis
/// difficulty about 1/512, `bits = 0x1f7fffff`), and an indexer using the same algorithm rebuilds the same state; an
/// indexer choosing the algorithm by the network params (Argon2id, D-094) considers these headers' PoW invalid, and its
/// main chain stops before the first such block.
#[test]
fn another_pow_algorithm_is_indexed_with_the_same_algorithm() {
    let (alice, bob) = (user("t12/alice"), user("t12/bob"));
    let (mock, mut genesis) = setup(&[(&alice, 1_000)]);
    genesis.bits = Some(0x1f7f_ffff);
    let algorithm: Arc<dyn ksana_consensus::PowAlgorithm> = Arc::new(Sha256Be);
    let a = Side::with_algorithm(&mock, &genesis, "t12", Some(algorithm.clone()));
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.submit(transfer(&alice, 1, token(3), bob.account, 5));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer_with_algorithm(dir.path(), &genesis, &mock, algorithm);
    catch_up(&ix);
    assert_eq!(ix.status().height, 2);
    assert_matches_node(&ix, &a);

    // each block also happens to satisfy Argon2id with probability about 1/512: at least one does not, and the indexer
    // stops before it
    let dir = tempfile::tempdir().unwrap();
    let default = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&default);
    assert!(default.status().height < ix.status().height);
    assert_eq!(default.status().halted, None);
}

/// DAG (D-063, found by soak run 36872025271): the merged parallel block Y carries a settlement transaction, and the
/// node broadcasts its settlement once the main-chain block merging it has `SETTLEMENT_DEPTH` confirmations; the
/// main-chain block X at the same height and the blocks after it are empty, so their headers only land with the next
/// block carrying a settlement transaction. While only Y is on BTC it is the tip of the indexer's main chain, but
/// without `SETTLEMENT_DEPTH` confirmations, so the indexer does not execute it as a main-chain block; once X's chain
/// lands, execution follows the node's main chain.
#[test]
fn a_merged_block_settled_before_the_chain_block_is_not_executed_as_one() {
    let (alice, bob) = (user("t13/alice"), user("t13/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t13a");
    let b = Side::new(&mock, &genesis, "t13b");

    // A's block Y (height 1) carries a path B op and a settlement transaction; B does not know it and first produces
    // two empty blocks X, X′
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    let y = a.mine();
    assert!(y.settlement.is_some());
    let x = b.mine_empty();
    let x2 = b.mine_empty();
    // after the exchange, B's next block Z has X′ as selected parent and merges Y; A switches to X's chain too
    b.accept(&y);
    let z = b.mine_empty();
    assert_eq!(z.header.parents[0], x2.header.hash());
    assert!(z.header.parents.contains(&y.header.hash()));
    for block in [&x, &x2, &z] {
        a.accept(block);
    }
    assert_eq!(a.block(1).header.hash(), x.header.hash());
    // Z reaches SETTLEMENT_DEPTH confirmations: A broadcasts Y's settlement (X, X′, Z and the empty blocks after them
    // carry no settlement transaction)
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    // the indexer sees only Y: it is the main-chain tip (height 1) but has no SETTLEMENT_DEPTH confirmations, so it is
    // not executed
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let progress = catch_up(&ix);
    assert_eq!(progress.main_height, 1);
    assert_eq!(progress.height, 0);
    assert_eq!(ix.find_block(&y.header.hash()), None);

    // the next block with a settlement transaction carries the headers of X, X′, Z and the following empty blocks: the
    // indexer executes along the node's main chain, Y is merged by Z
    a.submit(transfer(&alice, 1, token(2), bob.account, 1));
    let carrier = a.mine();
    assert!(carrier.settlement.is_some());
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.block_at(1).map(|(hash, _)| hash), Some(x.header.hash()));
    assert_eq!(ix.find_block(&y.header.hash()), None);
    assert!(ix.status().height >= z.header.height);
    assert_matches_node(&ix, &a);
}

/// Settlement batches (D-077): while block 1's settlement transaction is in flight, blocks 2, 3 carry no settlement
/// transaction; the next block after it confirms carries the batch settlement transaction, whose payload carries the
/// headers and bodies of blocks 2, 3. The indexer takes the bodies from the batch payload, and the state roots agree
/// with the node's block by block.
#[test]
fn bodies_carried_by_a_batch_settlement_are_indexed() {
    let (alice, bob) = (user("t12/alice"), user("t12/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::batched(&mock, &genesis, "t12");
    // 1: no settlement transaction in flight, carries its own settlement transaction
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    let b1 = a.mine();
    assert!(b1.settlement.is_some());
    // 2, 3: block 1's settlement transaction is in flight, the bodies back up
    a.submit(transfer(&alice, 1, token(3), bob.account, 10));
    let b2 = a.mine();
    a.submit(transfer(&alice, 2, token(3), bob.account, 10));
    let b3 = a.mine();
    assert!(b2.settlement.is_none() && b3.settlement.is_none());
    a.broadcast_settlements();
    mock.mine(1);
    // 4: the batch settlement transaction carries the bodies of blocks 2, 3
    let b4 = a.mine();
    let pair = SettlementPair::decode(b4.settlement.as_ref().unwrap()).unwrap();
    let payload = ksana_btc::payload::attached_payload(&PARAMS, &pair).unwrap();
    let payload = ksana_primitives::Payload::decode(&payload, &PARAMS).unwrap();
    let carried: Vec<[u8; 32]> = ksana_btc::payload::bodies_in(&payload)
        .iter()
        .map(|(hash, _)| *hash)
        .collect();
    assert!(carried.contains(&b2.header.hash()) && carried.contains(&b3.header.hash()));
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    // one more op block carries the headers of blocks 5, 6; once confirmed, the settled prefix passes block 3
    a.submit(transfer(&alice, 3, token(3), bob.account, 10));
    assert!(a.mine().settlement.is_some());
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert!(ix.status().height >= b3.header.height, "{:?}", ix.status());
    assert_matches_node(&ix, &a);
}

/// BTC-anchored finality (D-082): A's block 1 is final once its data has `BTC_FINALITY_CONFS` confirmations on BTC, and
/// the indexer executes it without waiting for `SETTLEMENT_DEPTH` confirmations; B's heavier chain (produced in
/// isolation before A settled) lands only afterwards, and neither the indexer nor the node re-selects the main chain;
/// A keeps producing blocks, and the indexer follows A's chain.
#[test]
fn a_finalized_block_pins_the_indexer_to_its_chain() {
    let (alice, bob, carol) = (user("t15/alice"), user("t15/bob"), user("t15/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t15a");
    let b = Side::new(&mock, &genesis, "t15b");

    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    // B's heavier chain (the same nonce spent to carol) is produced in isolation first and not settled yet: once A's
    // settlement lands, B would rebuild A's block 1 from the payload and switch to it as soon as it synced BTC (D-093),
    // so B does not sync any more from here on
    for _ in 0..5 {
        b.mine_empty();
    }
    b.submit(transfer(&alice, 0, token(4), carol.account, 20));
    b.mine();
    b.mine_empty();
    b.mine_empty();
    assert!(b.chain.tip().unwrap().chain_work > a.chain.tip().unwrap().chain_work);

    a.broadcast_settlements();
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    let a1 = a.block(1).header.hash();
    assert_eq!(a.chain.finalized().unwrap(), (a1, 1));
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    let status = ix.status();
    assert_eq!((status.finalized_height, status.finalized_hash), (1, a1));
    assert_eq!(
        status.height, 1,
        "a final block no longer needs SETTLEMENT_DEPTH confirmations"
    );
    assert_matches_node(&ix, &a);

    // B's chain settles and lands only now (broadcast directly without syncing BTC): the indexer's main chain still
    // passes through A's block 1
    b.chain.process_settlements().unwrap();
    mock.mine(1);
    let progress = catch_up(&ix);
    assert_eq!(ix.block_at(1).map(|(hash, _)| hash), Some(a1));
    assert_eq!(ix.status().finalized_hash, a1);
    assert_eq!(
        progress.main_height, 1,
        "the main chain is still A's chain (its blocks 2, 3 are empty and their headers have not landed yet)"
    );
    // node A does not switch either on receiving B's chain and keeps producing blocks; the indexer advances along A's
    // chain
    for height in 1..=b.tip_height() {
        a.accept(&b.block(height));
    }
    a.sync();
    assert_eq!(a.tip_height(), 3);
    a.submit(transfer(&alice, 1, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status().height, 2);
    assert_matches_node(&ix, &a);
    // BTC_FINALITY_CONFS more BTC blocks: A's block 4 (the headers of blocks 2, 3 landed with it) is final, the indexer
    // executes up to it
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    assert_eq!(a.chain.finalized().unwrap().1, 4);
    catch_up(&ix);
    assert_eq!(ix.status().height, 4);
    assert_matches_node(&ix, &a);
}

/// The data of a forged block (wrong `state_root`) gets `BTC_FINALITY_CONFS` confirmations on BTC: it meets the
/// finality condition but is invalid, so the finality decision excludes it together with the blocks built on it
/// (D-101) and the final tip stays on the node's chain; this holds for an indexer already running (which has marked it
/// as unable to land), for a freshly opened one (which judges it when the decision reaches it) and for the node (which
/// never had it and judges it from the data on BTC). Previously the final chain passed through it and the indexer
/// could only halt.
#[test]
fn an_invalid_block_landed_on_btc_is_never_final() {
    let (alice, bob) = (user("t16/alice"), user("t16/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t16");
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    let entries = vec![Entry::Sponsored {
        op: transfer(&alice, 1, token(2), bob.account, 1),
    }];
    let forged = forge(&a, &mock, entries, [0x55; 32], "t16/forger");
    confirm_forged(&a, &mock, &forged, "t16/forger");

    // an indexer already running: first marks the forged block as unable to land and re-selects the node's chain
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert_eq!(ix.status().halted, None);
    assert_eq!(ix.status().height, 1);
    // the forged block's data gets BTC_FINALITY_CONFS more confirmations: blocks 1 to 3 are final, the forged block is
    // excluded
    mock.mine(PARAMS.btc_finality_confs);
    catch_up(&ix);
    let block3 = a.block(3).header.hash();
    let status = ix.status();
    assert_eq!(status.halted, None);
    assert_eq!(
        (status.finalized_height, status.finalized_hash),
        (3, block3)
    );
    assert_eq!(status.height, 3);
    assert_eq!(ix.find_block(&forged.hash()), None);
    assert_matches_node(&ix, &a);

    // a freshly opened indexer: blocks 1 to 3 are final and are executed and checked as usual; the forged block fails
    // when the decision or the processing reaches it, and is excluded
    let dir = tempfile::tempdir().unwrap();
    let fresh = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&fresh);
    let status = fresh.status();
    assert_eq!(status.halted, None);
    assert_eq!(
        (status.finalized_height, status.finalized_hash),
        (3, block3)
    );
    assert_eq!(status.height, 3);
    assert_eq!(fresh.find_block(&forged.hash()), None);
    assert_matches_node(&fresh, &a);

    // the node, which never had the forged block, judges it from the data on BTC and agrees
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (block3, 3));
}

/// Attacker M puts the two headers of a chain heavier than A's landed part on BTC and withholds block 1's body (it
/// carries a transfer, so it needs a body). Node A does not have M's blocks: by the data on BTC its data has not landed
/// yet, so A's finality decision does not stop on it and does not request it from peers (§6.5 item 3, D-093, the same
/// criterion as the indexer's); once A's chain settles, the node and the indexer alike judge it final with the same
/// state, and M's blocks are never final. Previously the node would wait forever for M's block 1 while the indexer
/// advanced as usual, so their final tips disagreed.
#[test]
fn a_header_only_block_with_a_withheld_body_stalls_neither_the_node_nor_the_indexer() {
    let (alice, bob, carol) = (user("t17/alice"), user("t17/bob"), user("t17/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t17a");
    let m = Side::new(&mock, &genesis, "t17m");

    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    // M produces block 1 (the same nonce spent to carol) and an empty block in isolation, heavier than A's landed
    // part; it does not sync BTC afterwards (otherwise it would rebuild A's blocks from A's settlement payload)
    m.submit(transfer(&alice, 0, token(4), carol.account, 20));
    let x1 = m.mine();
    let x2 = m.mine_empty();
    assert!(x1.settlement.is_some(), "block 1 needs a body");
    a.broadcast_settlements();
    mock.mine(1);
    mock.submit(headers_tx(
        &mock,
        vec![x1.header.clone(), x2.header.clone()],
    ))
    .unwrap();
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    // neither the node nor the indexer judges any block final (the candidate is M's heavier chain, whose data has not
    // landed yet), and neither stops on M's block 1
    assert_eq!(a.chain.finalized().unwrap().1, 0);
    assert_eq!(
        a.chain.finality_evaluated_height().unwrap(),
        Some(mock.tip_height()),
        "the node's decision advances to the BTC tip"
    );
    assert!(
        a.chain.missing_parents().unwrap().is_empty(),
        "the node does not wait for M's blocks"
    );
    assert_eq!(ix.status().finalized_height, 0);
    assert_matches_node(&ix, &a);

    // A keeps producing blocks and settles: A's chain is heavier; after BTC_FINALITY_CONFS confirmations the node and
    // the indexer alike judge block 4 final, and the indexer executes up to it
    a.submit(transfer(&alice, 1, token(3), bob.account, 5));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    let a4 = a.block(4).header.hash();
    assert_eq!(a.chain.finalized().unwrap(), (a4, 4));
    assert!(!a.chain.is_final(&x1.header.hash(), 1).unwrap());
    catch_up(&ix);
    let status = ix.status();
    assert_eq!((status.finalized_height, status.finalized_hash), (4, a4));
    assert_eq!(status.height, 4);
    assert_matches_node(&ix, &a);
}

/// The indexer chooses its main chain from the headers on BTC alone, and a block it can process needs only
/// `SETTLEMENT_DEPTH` confirmations on that header-only DAG, which an attacker can supply with its own blocks. In one BTC
/// block the attacker lands a republish payload with the node's headers that have not landed yet, the header of B on
/// the node's tip (a path B op with a broken signature, `entries_root` matching that body), the headers of C and D on B
/// (only for the confirmations) and B's body. Honest nodes reject B (D-063); the indexer marks it like a block that
/// cannot land and re-selects the main chain instead of halting, and once the node's heavier chain lands in the next BTC
/// block it follows that chain and keeps verifying state roots, agreeing with the node; B never becomes final.
#[test]
fn an_invalid_body_on_a_header_only_chain_is_disqualified_without_halting() {
    let (alice, bob) = (user("t18/alice"), user("t18/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t18");
    // the node's blocks 1 (path B, its settlement lands), 2, 3 (empty, their headers not on BTC yet)
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();

    // the attacker's B on the node's tip (block 3), whose body is a path B op with a broken signature, and C, D on B
    let body = vec![broken_op(&alice, 1, bob.account)];
    let b = forge_header(&a, &mock, &body, [0x77; 32], "t18/forger");
    let snapshot = a.chain.btc().snapshot().unwrap();
    let mut headers = a
        .chain
        .payload_carry(&b.parents, &snapshot)
        .unwrap()
        .headers;
    assert_eq!(
        headers.len(),
        2,
        "the headers of blocks 2, 3 have not landed yet"
    );
    let b_index = u32::try_from(headers.len()).unwrap();
    headers.push(b.clone());
    headers.extend(forged_confirmations(&a, &b, "t18/forger"));
    let attack = republish_tx(&mock, headers, vec![(b_index, body)]);

    // meanwhile the node, which never sees B, produces block 4 (path B) and the empty blocks 5 to 7, heavier than B, C, D
    a.submit(transfer(&alice, 1, token(2), bob.account, 2));
    a.mine();
    for _ in 0..3 {
        a.mine_empty();
    }
    let honest = headers_tx(&mock, (5..=7).map(|h| a.block(h).header).collect());

    // BTC height N, only the attacker's payload: the indexer's main chain runs through B, C, D and B is processable; its
    // body is invalid, so B is marked and the main chain goes back to the node's (tip block 3, with SETTLEMENT_DEPTH
    // confirmations up to block 1)
    mock.mine_block(vec![attack]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let progress = catch_up(&ix);
    let status = ix.status();
    assert_eq!(
        status.halted, None,
        "an invalid block that is not BTC-final must not halt the indexer"
    );
    assert_eq!(progress.main_height, 3);
    assert_eq!(status.height, 1);
    assert_eq!(ix.find_block(&b.hash()), None);
    assert_matches_node(&ix, &a);

    // BTC height N + 1: block 4's settlement and the headers of blocks 5 to 7 land, and the indexer follows the node's
    // chain
    a.broadcast_settlements();
    mock.submit(honest).unwrap();
    mock.mine(1);
    let progress = catch_up(&ix);
    assert_eq!(progress.main_height, 7);
    assert_eq!(ix.status().height, 5);
    assert_matches_node(&ix, &a);

    // BTC_FINALITY_CONFS more BTC blocks: the node's chain is final up to block 7, B never is; the indexer executes and
    // verifies up to block 7
    mock.mine(PARAMS.btc_finality_confs);
    catch_up(&ix);
    let status = ix.status();
    assert_eq!((status.finalized_height, status.height), (7, 7));
    assert_eq!(ix.find_block(&b.hash()), None);
    assert_matches_node(&ix, &a);
}

/// A path B op from `alice` whose signature is broken (the rest of the op is valid).
fn broken_op(alice: &User, nonce: u64, to: [u8; 32]) -> Entry {
    let mut op = transfer(alice, nonce, token(2), to, 1);
    match &mut op.sig {
        SigScheme::Bip340(sig) => sig[0] ^= 1,
        other => panic!("unexpected signature scheme {other:?}"),
    }
    Entry::Sponsored { op }
}

/// Forges a header on `parents` (the first must be the selected parent, i.e. have the greatest (blue work, hash)) in
/// `dag`, a header DAG holding the headers so far: the weight, GHOSTDAG data and `bits` as the nodes compute them,
/// valid PoW. The header is added to `dag`.
fn forge_in(
    dag: &mut HeaderDag,
    consensus: &PowConsensus,
    parents: Vec<[u8; 32]>,
    btc_anchor: BtcAnchor,
    state_root: [u8; 32],
    seed: &str,
) -> BlockHeader {
    let sp = dag.header(&parents[0]).unwrap().clone();
    let timestamp = parents
        .iter()
        .map(|p| dag.header(p).unwrap().timestamp)
        .max()
        .unwrap()
        + 1;
    let mut header = BlockHeader {
        version: BLOCK_VERSION,
        height: sp.height + 1,
        parents,
        btc_anchor,
        entries_root: ksana_crypto::entries_root(&[]).unwrap(),
        state_root,
        producer: user(seed).account,
        timestamp,
        bits: 0,
        nonce: 0,
    };
    let sp_streak = dag.dag().parent_streak(&header.parents).unwrap();
    let weight = consensus.weight(&header, sp_streak);
    let data = dag.dag().compute(&header.parents, weight).unwrap();
    assert_eq!(
        data.selected_parent,
        Some(sp.hash()),
        "the first parent must be the selected parent"
    );
    header.bits = consensus.next_bits(&sp, data.daa_score, weight).unwrap();
    assert!(consensus.solve(&mut header, weight, 0, 1 << 24).unwrap());
    assert_eq!(
        dag.insert(consensus, header.clone(), &BtcSnapshot::default()),
        Insert::Connected(1)
    );
    header
}

/// The same attack through a merge: the attacker's B (invalid body) is a sibling of the node's block 4, and the node
/// has already built block 5 on block 4, so the attacker forges X with block 5 as its selected parent (more blue work
/// than B: a block's blue work does not include its own work, so B and block 4 alone would tie and the hash would
/// decide) and B as a merged parent, plus Y and Z on X for the confirmations, and lands them in one republish payload
/// with B's body. The nodes reject X too (its past contains B, D-063): the indexer marks X, the block it was executing,
/// and goes back to the node's chain instead of halting, then keeps following it.
#[test]
fn a_block_merging_an_invalid_block_is_disqualified_without_halting() {
    let (alice, bob) = (user("t19/alice"), user("t19/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "t19");
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();

    // B on block 3 (invalid body); the node's blocks 4 and 5, empty, on block 3
    let body = vec![broken_op(&alice, 1, bob.account)];
    let b = forge_header(&a, &mock, &body, [0x77; 32], "t19/forger");
    let b4 = a.mine_empty().header;
    assert_eq!(b4.parents, b.parents);
    let b5 = a.mine_empty().header;
    // X on block 5 merging B, then Y and Z, built in the attacker's own header DAG
    let consensus = a.chain.consensus();
    let mut dag = HeaderDag::new(&PARAMS, consensus, a.chain.genesis().clone()).unwrap();
    for header in (1..=5).map(|h| a.block(h).header).chain([b.clone()]) {
        assert_eq!(
            dag.insert(consensus, header, &BtcSnapshot::default()),
            Insert::Connected(1)
        );
    }
    let (btc_height, btc_hash) = mock.tip().unwrap();
    let anchor = BtcAnchor {
        height: btc_height,
        hash: btc_hash,
    };
    let x = forge_in(
        &mut dag,
        consensus,
        vec![b5.hash(), b.hash()],
        anchor.clone(),
        [0x78; 32],
        "t19/forger",
    );
    assert_eq!(
        dag.ghostdag(&x.hash()).unwrap().mergeset_ordered,
        vec![b.hash()]
    );
    let y = forge_in(
        &mut dag,
        consensus,
        vec![x.hash()],
        anchor.clone(),
        x.state_root,
        "t19/forger",
    );
    let z = forge_in(
        &mut dag,
        consensus,
        vec![y.hash()],
        anchor,
        x.state_root,
        "t19/forger",
    );
    // one republish payload: the node's headers not landed yet (blocks 2 to 5), B, X, Y, Z and B's body
    let snapshot = a.chain.btc().snapshot().unwrap();
    let mut headers = a
        .chain
        .payload_carry(&[b5.hash()], &snapshot)
        .unwrap()
        .headers;
    assert_eq!(
        headers.len(),
        4,
        "the headers of blocks 2 to 5 have not landed yet"
    );
    let b_index = u32::try_from(headers.len()).unwrap();
    headers.extend([b.clone(), x.clone(), y, z]);
    mock.mine_block(vec![republish_tx(&mock, headers, vec![(b_index, body)])])
        .unwrap();

    // the main chain ran through X (X, Y and Z on top of block 5); executing X checks the block it merges, B is
    // invalid, X is marked: back on the node's chain (tip block 5, with SETTLEMENT_DEPTH confirmations up to block 3)
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let progress = catch_up(&ix);
    let status = ix.status();
    assert_eq!(
        status.halted, None,
        "a block merging an invalid block must not halt the indexer while it is not BTC-final"
    );
    assert_eq!(progress.main_height, 5);
    assert_eq!(status.height, 3);
    assert_eq!(ix.find_block(&x.hash()), None);
    assert_matches_node(&ix, &a);

    // the node keeps producing and settling blocks: the indexer follows its chain (block 6 settles, the block with
    // SETTLEMENT_DEPTH confirmations is block 4)
    a.submit(transfer(&alice, 1, token(2), bob.account, 2));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    catch_up(&ix);
    assert_eq!(ix.status().height, 4);
    assert_matches_node(&ix, &a);
}

/// How the forged block of [`forged_anchor_run`] lands.
#[derive(Clone, Copy, Debug)]
enum Forgery {
    /// With its body, in a settlement transaction pair; its `state_root` is wrong.
    Settled,
    /// Only its header, in a republish payload: its body (a transfer) never lands.
    HeaderOnly,
    /// An empty block on the node's block 3 merging the forged block, which is on block 1 and whose body (a transfer)
    /// never lands; both headers in one republish payload. The anchor of the empty block is always the real one.
    Merged,
}

/// The review of D-101: the node's blocks 1 to 3 have landed, its blocks 4 to 6 have not (the fee race of the
/// path assessment). A forged block on the node's chain lands as `forgery` says, anchored at the BTC tip with the real
/// hash or, when `fabricated`, with a hash no BTC block has; its header gets `BTC_FINALITY_CONFS` confirmations with
/// nothing heavier landed. Then the node's chain lands, goes on and gets confirmations. Checks that the node and a
/// fresh indexer agree, that the node's decision has evaluated up to the BTC tip and that no forged block is final;
/// returns (node final height, indexer final height, node tip height, node settled height).
fn forged_anchor_run(forgery: Forgery, fabricated: bool) -> (u64, u64, u64, u64) {
    let tag = format!("t20/{forgery:?}/{fabricated}");
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, &tag);
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    let (block1, block3) = (a.block(1).header, a.block(3).header);
    a.submit(transfer(&alice, 1, token(2), bob.account, 1));
    a.mine();
    a.mine_empty();
    a.mine_empty();

    let seed = format!("{tag}/forger");
    let real = btc_tip_anchor(&mock);
    let anchor = if fabricated {
        BtcAnchor {
            height: real.height,
            hash: [0xab; 32],
        }
    } else {
        real.clone()
    };
    let entries = vec![Entry::Sponsored {
        op: transfer(&alice, 1, token(2), bob.account, 2),
    }];
    let snapshot = a.chain.btc().snapshot().unwrap();
    let forged = match forgery {
        Forgery::Settled => {
            let x = forge_header_on(&a, &block3, anchor, &entries, [0x55; 32], &seed);
            settle_forged(&a, &mock, &x, &entries, &seed);
            vec![x]
        }
        Forgery::HeaderOnly => {
            let x = forge_header_on(&a, &block3, anchor, &entries, [0x55; 32], &seed);
            let mut headers = a
                .chain
                .payload_carry(&x.parents, &snapshot)
                .unwrap()
                .headers;
            headers.push(x.clone());
            mock.mine_block(vec![headers_tx(&mock, headers)]).unwrap();
            vec![x]
        }
        Forgery::Merged => {
            let f = forge_header_on(&a, &block1, anchor, &entries, [0x55; 32], &seed);
            let consensus = a.chain.consensus();
            let mut dag = HeaderDag::new(&PARAMS, consensus, a.chain.genesis().clone()).unwrap();
            for header in (1..=3).map(|h| a.block(h).header).chain([f.clone()]) {
                assert_eq!(
                    dag.insert(consensus, header, &BtcSnapshot::default()),
                    Insert::Connected(1)
                );
            }
            let x = forge_in(
                &mut dag,
                consensus,
                vec![block3.hash(), f.hash()],
                real,
                [0x56; 32],
                &seed,
            );
            assert_eq!(
                dag.ghostdag(&x.hash()).unwrap().mergeset_ordered,
                vec![f.hash()]
            );
            let mut headers = a
                .chain
                .payload_carry(&[block3.hash()], &snapshot)
                .unwrap()
                .headers;
            headers.extend([f.clone(), x.clone()]);
            mock.mine_block(vec![headers_tx(&mock, headers)]).unwrap();
            vec![f, x]
        }
    };
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    assert_eq!(
        a.chain.finalized().unwrap().1,
        3,
        "{forgery:?}, {fabricated}"
    );

    // The node's heavier chain lands, goes on and gets confirmations
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    for nonce in 2..4 {
        a.submit(transfer(&alice, nonce, token(2), bob.account, 1));
        a.mine();
        a.mine_empty();
        a.mine_empty();
        a.broadcast_settlements();
        mock.mine(1);
        a.sync();
    }
    mock.mine(PARAMS.btc_finality_confs + 2);
    a.sync();
    assert_eq!(
        a.chain.finality_evaluated_height().unwrap(),
        Some(mock.tip_height()),
        "the node's decision does not wait ({forgery:?}, {fabricated})"
    );
    for header in &forged {
        assert!(!a.chain.is_final(&header.hash(), header.height).unwrap());
    }
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    (
        a.chain.finalized().unwrap().1,
        ix.status().finalized_height,
        a.tip_height(),
        a.settled_height(),
    )
}

/// The review of D-101: a block whose `btc_anchor` names no BTC block (a hash no BTC block has, at a height
/// on the chain) lands as the heaviest candidate and gets `BTC_FINALITY_CONFS` confirmations. With its body landed and
/// a wrong `state_root`, its re-execution in anchor-invalid mode failed at every BTC tip, which the first version of
/// D-101 took as no verdict yet; with its body never on BTC, its forced-inclusion entries cannot be derived on an
/// anchor chain that does not exist, so its body and whether its data has landed could never be determined (D-093).
/// Either way the finality decision of the node and of every indexer waited at that BTC height for good, and the node's
/// chain landing later never connected. The block is now excluded for its anchor before anything else is asked about
/// it, and everything ends as when it has the real anchor hash.
#[test]
fn a_block_anchored_on_no_btc_block_is_excluded_and_finality_goes_on() {
    for forgery in [Forgery::Settled, Forgery::HeaderOnly] {
        let real = forged_anchor_run(forgery, false);
        assert_eq!(real, (10, 10, 12, 10), "{forgery:?}");
        assert_eq!(forged_anchor_run(forgery, true), real, "{forgery:?}");
    }
}

/// The same through a merge: the heaviest candidate is an empty block with a real anchor that merges a block anchored
/// on no BTC block whose body never lands. The candidate's closure waited for the merged block's landing status, which
/// can never be determined; the candidate is now excluded for the anchor of the block it merges.
#[test]
fn a_block_merging_a_block_anchored_on_no_btc_block_is_excluded() {
    let real = forged_anchor_run(Forgery::Merged, false);
    assert_eq!(real, (10, 10, 12, 10));
    assert_eq!(forged_anchor_run(Forgery::Merged, true), real);
}
