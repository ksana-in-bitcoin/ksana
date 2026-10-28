//! M15: BTC-anchored finality (D-082) and the anchor time bound of timestamps (D-083), without the network: several
//! node instances share the same mock BTC chain, each produces blocks, then hands its blocks to the others.
//!
//! - Once a block's data (together with the data of the blocks it merges) has `BTC_FINALITY_CONFS` confirmations on
//!   BTC, the block is final: a heavier chain appearing only afterwards cannot reorg it out; a node that chose another
//!   chain switches to it as soon as it gets it, even with less work.
//! - When both chains settle in time, the choice is by work and the heavier one becomes final; a node that does not
//!   yet have the finalized block rebuilds it from the BTC payloads (D-093), and accepts it as usual when a peer
//!   provides it.
//! - A block whose timestamp exceeds the anchor BTC block's time plus `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` is invalid (also
//!   checked when syncing historical blocks).
//! - When a lighter competing chain appears on BTC, the node republishes its own headers immediately (D-091) without
//!   waiting `4 × SETTLEMENT_TIMEOUT`, so the heavier chain becomes final first; a chain finality can never decide,
//!   such as one anchored off the BTC best chain whose settlement a BTC reorg confirmed again, does not count, while
//!   the blocks below its first block anchored off the chain still race this node's chain and set off republishing
//!   (D-104).

mod common;

use common::side::{setup, Side};
use common::{transfer_op, user};
use std::sync::Arc;

use ksana_btc::client::BtcClient;
use ksana_btc::mock::MockBtc;
use ksana_consensus::Consensus;
use ksana_node::chain::Accepted;
use ksana_node::rescue::Rescuer;
use ksana_node::NodeError;
use ksana_primitives::{Block, BlockHeader, Payload, DEVNET};

/// Broadcasts the settlement transactions of blocks that have reached `SETTLEMENT_DEPTH` and gives them `confs`
/// confirmations on BTC.
fn settle(side: &Side, mock: &MockBtc, confs: u32) {
    side.sync();
    side.chain.process_settlements().unwrap();
    mock.mine(confs);
    side.sync();
}

/// The header republish payloads in the mock chain's mempool.
fn republished(mock: &MockBtc) -> Vec<Vec<BlockHeader>> {
    mock.mempool()
        .iter()
        .filter_map(|tx| match ksana_btc::data::tx_payload(&DEVNET, tx) {
            Some((_, Payload::Headers { headers, .. })) => Some(headers),
            _ => None,
        })
        .collect()
}

/// Only after A's block 1 is final does B reveal a heavier, never-settled chain (produced in isolation before A
/// settled): A does not switch; when syncing BTC, B rebuilds the finalized block 1 from A's settlement payload (D-093)
/// and switches to A's chain, getting the later blocks over the network.
#[test]
fn a_finalized_block_survives_a_heavier_chain_revealed_late() {
    let (alice, bob, carol) = (user("m15/alice1"), user("m15/bob1"), user("m15/carol1"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m15a1");
    let b = Side::new(&mock, &genesis, "m15b1");

    // A: block 1 transfers to bob, then two more blocks so it reaches SETTLEMENT_DEPTH
    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    let a_blocks: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    let a1 = a_blocks[0].header.hash();
    // B: spends the same nonce to carol on a heavier chain, never settled (once A's settlement lands on BTC, B would
    // rebuild A's blocks from BTC, so produce it beforehand)
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let b_blocks: Vec<Block> = (0..6).map(|_| b.mine()).collect();
    assert!(b.chain.tip().unwrap().chain_work > a.chain.tip().unwrap().chain_work);
    assert_eq!(b.ksa(&carol), 20);
    // A's settlement lands on BTC and gets BTC_FINALITY_CONFS confirmations
    settle(&a, &mock, DEVNET.btc_finality_confs);
    assert_eq!(a.chain.finalized().unwrap(), (a1, 1));
    assert!(a.chain.is_final(&a1, 1).unwrap());
    assert!(!a.chain.is_final(&a_blocks[1].header.hash(), 2).unwrap());
    assert_eq!(a.ksa(&bob), 10);

    // A receives B's chain: even with more work it is only a side chain, the main chain still passes through the
    // finalized block 1
    for block in &b_blocks {
        let report = a.accept(block).unwrap();
        assert!(
            matches!(report.outcome, Accepted::Side),
            "{:?}",
            report.outcome
        );
    }
    a.sync();
    assert_eq!(a.chain.tip().unwrap().hash, a_blocks[2].header.hash());
    assert_eq!(a.ksa(&bob), 10);
    assert_eq!(a.ksa(&carol), 0);
    // A keeps producing: not with B's tip as a parent (it is heavier than the main-chain tip and cannot be merged)
    let a4 = a.mine();
    assert_eq!(a4.header.parents, vec![a_blocks[2].header.hash()]);

    // B syncs BTC: by work A's block 1 is only a side chain, but it is what is final on BTC; B does not have it,
    // rebuilds it from A's settlement payload (D-093) and switches over (the main-chain changes are handed to the entry
    // pool maintenance as a node would), and the later blocks arrive over the network and extend it
    while b.chain.sync_btc().unwrap() {}
    let (connected, disconnected) = b.chain.take_chain_changes().unwrap();
    assert_eq!(
        connected
            .iter()
            .map(|b| b.header.hash())
            .collect::<Vec<_>>(),
        vec![a1]
    );
    assert_eq!(disconnected.len(), 6);
    ksana_node::ops::chain_changed(
        &b.chain,
        &b.mempool,
        &b.producer.events,
        &connected,
        &disconnected,
    )
    .unwrap();
    assert_eq!(b.chain.tip().unwrap().hash, a1);
    assert_eq!(b.chain.finalized().unwrap(), (a1, 1));
    assert!(matches!(
        b.accept(&a_blocks[0]).unwrap().outcome,
        Accepted::Duplicate
    ));
    for block in a_blocks[1..].iter().chain([&a4]) {
        let report = b.accept(block).unwrap();
        assert!(
            matches!(report.outcome, Accepted::Extended(_)),
            "{:?}",
            report.outcome
        );
    }
    b.sync();
    assert_eq!(b.chain.finalized().unwrap(), (a1, 1));
    assert_eq!(b.chain.tip().unwrap().hash, a4.header.hash());
    assert_eq!(b.ksa(&bob), 10);
    assert_eq!(b.ksa(&carol), 0);
    for block in &b_blocks {
        assert_eq!(b.chain.accepted_height(&block.header.hash()).unwrap(), None);
    }
    // B's later block attaches to A's chain, and the two nodes agree
    let b_next = b.mine();
    assert_eq!(b_next.header.parents, vec![a4.header.hash()]);
    assert!(matches!(
        a.accept(&b_next).unwrap().outcome,
        Accepted::Extended(_)
    ));
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());
}

/// A finalized block is merged by a heavier chain (instead of staying as an isolated tip): the node that chose the
/// heavier chain must still switch to the chain passing through it once it is final — the block passing through it is
/// no longer a DAG tip, only a "selected-chain tip" (the selected parent of no block), and the main-chain candidates
/// must be taken from there.
#[test]
fn a_node_switches_to_a_finalized_block_its_heavier_chain_has_merged() {
    let (alice, bob, carol) = (user("m15/alice4"), user("m15/bob4"), user("m15/carol4"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m15a4");
    let b = Side::new(&mock, &genesis, "m15b4");

    // A: block 1 transfers to bob, then two more blocks so it reaches SETTLEMENT_DEPTH; the settlement lands on BTC
    // with only 1 confirmation (not final yet)
    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    let a_blocks: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    settle(&a, &mock, 1);
    let a1 = a_blocks[0].header.hash();
    let a3 = a_blocks[2].header.hash();
    assert_eq!(a.chain.finalized().unwrap().1, 0);

    // B: a heavier chain, never settled; after receiving A's chain it keeps its own as the main chain, and its next
    // block merges A's tip
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let b_blocks: Vec<Block> = (0..6).map(|_| b.mine()).collect();
    for block in &a_blocks {
        let report = b.accept(block).unwrap();
        assert!(
            matches!(report.outcome, Accepted::Side),
            "{:?}",
            report.outcome
        );
    }
    let merger = b.mine();
    assert_eq!(merger.header.parents[0], b_blocks[5].header.hash());
    assert!(
        merger.header.parents.contains(&a3),
        "{:?}",
        merger.header.parents
    );
    assert_eq!(b.ksa(&carol), 20);

    // A's block 1 data gets more confirmations and becomes final: B's main chain must change to the chain passing
    // through it — a3 is not a tip (merged by merger), yet it is the best block whose selected chain passes through
    // a1
    mock.mine(DEVNET.btc_finality_confs - 1);
    b.sync();
    assert_eq!(b.chain.finalized().unwrap(), (a1, 1));
    assert_eq!(b.chain.tip().unwrap().hash, a3);
    assert_eq!(b.ksa(&bob), 10);
    assert_eq!(b.ksa(&carol), 0);
    assert_eq!(
        b.chain.accepted_height(&merger.header.hash()).unwrap(),
        None
    );
    // B's later block attaches to a3: merger is heavier and cannot be a parent
    let b_next = b.mine();
    assert_eq!(b_next.header.parents, vec![a3]);

    // A receives all of B's blocks: all only side chain; the two nodes' tips agree
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (a1, 1));
    for block in b_blocks.iter().chain([&merger]) {
        let report = a.accept(block).unwrap();
        assert!(
            matches!(report.outcome, Accepted::Side),
            "{:?}",
            report.outcome
        );
    }
    assert!(matches!(
        a.accept(&b_next).unwrap().outcome,
        Accepted::Extended(_)
    ));
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());
}

/// Both chains settle in time: the choice is by the work of each chain's landed part on BTC, and the heavier chain
/// becomes final; a node that knows only the lighter one rebuilds the finalized blocks of the heavier chain from the
/// BTC payloads (D-093) and switches over. Empty blocks carry no settlement transaction (their headers are carried by
/// a later settlement payload), so B's blocks 1 and 2 both hold entries: their settlements confirm in the same BTC
/// block as A's block 1 settlement, and B's chain is heavier on BTC.
#[test]
fn finality_follows_the_heavier_chain_when_both_settle_in_time() {
    let (alice, bob, carol) = (user("m15/alice2"), user("m15/bob2"), user("m15/carol2"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m15a2");
    let b = Side::new(&mock, &genesis, "m15b2");

    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    let a_blocks: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let b1_block = b.mine();
    b.submit(transfer_op(&alice, 1, 4, carol.account, 5));
    let mut b_blocks = vec![b1_block, b.mine()];
    b_blocks.extend((0..2).map(|_| b.mine()));
    // The three settlements confirm in the same BTC block
    a.sync();
    a.chain.process_settlements().unwrap();
    b.sync();
    b.chain.process_settlements().unwrap();
    mock.mine(DEVNET.btc_finality_confs);
    let (a1, b1, b2) = (
        a_blocks[0].header.hash(),
        b_blocks[0].header.hash(),
        b_blocks[1].header.hash(),
    );

    // A knows only its own chain: the data of B's heavier chain on BTC has enough confirmations, and A rebuilds B's
    // blocks 1 and 2 from the settlement payloads (D-093): the decision reaches block 2 and the main chain switches to
    // it (A's own chain is heavier by work and yields only because of finality)
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (b2, 2));
    assert_eq!(a.chain.tip().unwrap().hash, b2);
    assert!(a.chain.missing_parents().unwrap().is_empty());
    assert!(a.chain.is_final(&b1, 1).unwrap());
    assert!(!a.chain.is_final(&a1, 1).unwrap());
    assert_eq!(a.ksa(&carol), 25);
    // B knows its own chain: blocks 1 and 2 are both final
    b.sync();
    assert_eq!(b.chain.finalized().unwrap(), (b2, 2));
    assert!(b.chain.is_final(&b1, 1).unwrap());
    assert!(!b.chain.is_final(&a1, 1).unwrap());

    // A gets B's chain over the network: the first two blocks are already present, the last two extend it
    for (index, block) in b_blocks.iter().enumerate() {
        let report = a.accept(block).unwrap();
        if index < 2 {
            assert!(
                matches!(report.outcome, Accepted::Duplicate),
                "{:?}",
                report.outcome
            );
        } else {
            assert!(
                matches!(report.outcome, Accepted::Extended(_)),
                "{:?}",
                report.outcome
            );
        }
    }
    a.sync();
    assert_eq!(a.chain.tip().unwrap().hash, b_blocks[3].header.hash());
    assert_eq!(a.chain.finalized().unwrap(), (b2, 2));
    assert_eq!(a.ksa(&carol), 25);
    assert_eq!(a.ksa(&bob), 0);
    // That the finalized height appears consistently with the settlement info in the RPC results (`ksana_getChainInfo`,
    // `btc_final`) is covered by the M15 RPC tests
}

/// D-091: A produces only empty blocks from the genesis block (no settlement transaction, none of the headers landed
/// on BTC yet), and B settles a lighter chain with a transfer onto BTC. Without the republish trigger, A's chain would
/// republish only after `4 × SETTLEMENT_TIMEOUT` BTC blocks, B's block would reach `BTC_FINALITY_CONFS` confirmations
/// first and be judged final, and A's heavier chain would be discarded; with it, A republishes its own headers once
/// the competing chain has been there for a full BTC block and it still has no carrier of its own (without waiting for
/// the depth to reach `SETTLEMENT_DEPTH`), and A's chain becomes final. Less than one BTC block of presence does not
/// republish; a chain with blue work not below A's is not a competing chain; once A's chain lands on BTC the competing
/// chain no longer triggers.
#[test]
fn a_lighter_chain_on_btc_triggers_an_immediate_republish() {
    let (alice, carol) = (user("m15/alice5"), user("m15/carol5"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m15a5");
    let b = Side::new(&mock, &genesis, "m15b5");
    let rescuer = Rescuer::new(
        Arc::clone(&a.chain),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );

    // A: 7 empty blocks (weights 12, 24, 48, 96, 120, 120, 120), no settlement transaction, headers do not land on BTC,
    // and the republish condition is not met either
    let a_blocks: Vec<Block> = (0..7).map(|_| a.mine()).collect();
    let (a1, a_tip) = (a_blocks[0].header.hash(), a_blocks[6].header.hash());
    assert!(a_blocks
        .iter()
        .all(|b| b.entries.is_empty() && b.settlement.is_none()));
    rescuer.run_once().unwrap();
    assert!(
        republished(&mock).is_empty(),
        "no early republish without a competing chain"
    );
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.header_seen(&a1), None);
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), None);

    // B: block 1 transfers to carol, then two empty blocks so it reaches SETTLEMENT_DEPTH; the settlement lands on BTC:
    // a lighter chain has appeared on BTC (only B's block 1 header: the settlement transaction is assembled when the
    // block is produced, so the later empty blocks are not in it)
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let b_blocks: Vec<Block> = (0..3).map(|_| b.mine()).collect();
    let b1 = b_blocks[0].header.hash();
    assert!(b.chain.tip().unwrap().chain_work < a.chain.tip().unwrap().chain_work);
    settle(&b, &mock, 1);
    let landed = mock.tip_height();
    assert_eq!(b.ksa(&carol), 20);
    // A receives B's blocks (side chain) and syncs BTC: the competing chain is B's block 1
    for block in &b_blocks {
        assert!(matches!(a.accept(block).unwrap().outcome, Accepted::Side));
    }
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.header_seen(&b1), Some(landed));
    assert_eq!(snapshot.header_seen(&b_blocks[1].header.hash()), None);
    // Less than one BTC block of presence does not count: a parallel block landing first is normal, and this node's
    // next block with a settlement transaction usually carries its own headers right after
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), None);
    rescuer.run_once().unwrap();
    assert!(
        republished(&mock).is_empty(),
        "no republish within the first BTC block of presence"
    );
    // After one BTC block A still has no block with a settlement transaction: the competing chain is B's block 1
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), Some((b1, 1)));
    assert_eq!(
        a.chain.btc_competitor(&snapshot, 1).unwrap(),
        None,
        "only those that appeared within the last within BTC blocks are considered"
    );
    // B itself sees no competing chain: only its chain is on BTC
    b.sync();
    let b_snapshot = b.chain.btc().snapshot().unwrap();
    assert_eq!(b.chain.btc_competitor(&b_snapshot, 6).unwrap(), None);

    // A republishes immediately: without waiting for the depth to reach SETTLEMENT_DEPTH, the headers of blocks 1-7 are
    // all sent at once (the other side is racing for finality, and landing the tip block on BTC does no harm)
    rescuer.run_once().unwrap();
    let headers = republished(&mock);
    assert_eq!(headers.len(), 1, "{headers:?}");
    assert_eq!(
        headers[0].iter().map(|h| h.height).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6, 7]
    );
    assert_eq!(headers[0][0].hash(), a1);
    // The same run is not republished again
    rescuer.run_once().unwrap();
    assert_eq!(republished(&mock).len(), 1);

    // After the republish lands, the competing chain no longer triggers; the heaviest connected chain on BTC is A's:
    // B's block data does not become final when it reaches the confirmations, and A's 7 blocks become final when the
    // republish does
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.header_seen(&a_tip), Some(landed + 2));
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), None);
    rescuer.run_once().unwrap();
    assert!(republished(&mock).is_empty());
    mock.mine(DEVNET.btc_finality_confs - 3);
    a.sync();
    assert_eq!(
        a.chain.finalized().unwrap().1,
        0,
        "when B's block reaches the confirmations, A's chain is already on BTC, so B's block is not final"
    );
    assert!(!a.chain.is_final(&b1, 1).unwrap());
    mock.mine(2);
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (a_tip, 7));
    assert!(a.chain.is_final(&a1, 1).unwrap());
    assert!(!a.chain.is_final(&b1, 1).unwrap());
    assert_eq!(a.chain.tip().unwrap().hash, a_tip, "A does not switch");
    assert_eq!(a.ksa(&carol), 0);

    // B gets A's chain: heavier, and A's blocks are the final ones, so it switches over; B's transfer returns to the
    // entry pool
    for block in &a_blocks {
        b.accept(block).unwrap();
    }
    b.sync();
    assert_eq!(b.chain.tip().unwrap().hash, a_tip);
    assert_eq!(b.chain.finalized().unwrap(), (a_tip, 7));
    assert_eq!(b.ksa(&carol), 0);
}

/// D-104: a chain finality can never decide does not count as a competing chain. B's lighter chain lands on BTC and
/// counts (D-091); then a BTC reorg takes its anchor off the best chain, and its settlement transactions confirm again
/// on the new branch, putting the same headers onto BTC once more. A's own chain, anchored below the fork point, does
/// not move, and B's chain no longer sets off a republish: it could never be final, and every node with funding would
/// otherwise race to land a chain that keeps nothing.
#[test]
fn a_chain_anchored_off_the_btc_best_chain_is_no_competing_chain() {
    let (alice, carol) = (user("m15/alice8"), user("m15/carol8"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m15a8");
    let b = Side::new(&mock, &genesis, "m15b8");
    let rescuer = Rescuer::new(
        Arc::clone(&a.chain),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    // A: 7 empty blocks anchored below the fork point, none of them on BTC
    let a_blocks: Vec<Block> = (0..7).map(|_| a.mine()).collect();
    let a_tip = a_blocks[6].header.hash();
    let fork = mock.tip_height();

    // B: block 1 anchors X above the fork point, and its settlement lands in the next BTC block; once it has been
    // there for a full BTC block it is A's competing chain
    mock.mine(1);
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let b_blocks: Vec<Block> = (0..3).map(|_| b.mine()).collect();
    let b1 = b_blocks[0].header.hash();
    assert_eq!(b_blocks[0].header.btc_anchor.height, fork + 1);
    settle(&b, &mock, 1);
    for block in &b_blocks {
        assert!(matches!(a.accept(block).unwrap().outcome, Accepted::Side));
    }
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), Some((b1, 1)));

    // The BTC reorg: X goes, B's settlement transactions wait out the new branch's first block and land on its second,
    // above X's height, where their payload counts again
    let pair =
        ksana_btc::settlement::SettlementPair::decode(b_blocks[0].settlement.as_ref().unwrap())
            .unwrap();
    mock.reorg(3);
    mock.hold(&pair.commit_txid());
    mock.hold(&pair.txid());
    mock.mine(1);
    mock.release(&pair.commit_txid());
    mock.release(&pair.txid());
    mock.mine(3);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(!snapshot.contains(&b_blocks[0].header.btc_anchor));
    assert_eq!(snapshot.header_seen(&b1), Some(fork + 2));
    assert_eq!(
        a.chain.tip().unwrap().hash,
        a_tip,
        "A's chain is anchored below X"
    );
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), None);
    rescuer.run_once().unwrap();
    assert!(republished(&mock).is_empty());
}

/// D-104: the blocks below one anchored off the BTC best chain still compete. B's empty block 1 anchors at the fork
/// point and its block 2 on X above it, and block 2's settlement carries both headers onto BTC. A BTC reorg takes X off
/// the best chain and the settlement confirms again on the new branch: block 2 can never be final, but block 1 can, and
/// A's heavier chain, still off BTC, would go with it. Block 1 is A's competing chain, A republishes at once, and A's
/// chain becomes final.
#[test]
fn the_blocks_below_one_anchored_off_the_btc_best_chain_still_compete() {
    let (alice, carol) = (user("m15/alice9"), user("m15/carol9"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m15a9");
    let b = Side::new(&mock, &genesis, "m15b9");
    let rescuer = Rescuer::new(
        Arc::clone(&a.chain),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    // A: 7 empty blocks anchored at the fork point, none of them on BTC
    let a_blocks: Vec<Block> = (0..7).map(|_| a.mine()).collect();
    let a_tip = a_blocks[6].header.hash();
    let fork = mock.tip_height();

    // B: block 1 at the fork point, then block 2 with a transfer on X; once its settlement has been on BTC for a full
    // BTC block, block 2 is A's competing chain
    let first = b.mine();
    mock.mine(1);
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let b_blocks: Vec<Block> = std::iter::once(first)
        .chain((0..3).map(|_| b.mine()))
        .collect();
    let (b1, b2) = (b_blocks[0].header.hash(), b_blocks[1].header.hash());
    assert_eq!(b_blocks[0].header.btc_anchor.height, fork);
    assert_eq!(b_blocks[1].header.btc_anchor.height, fork + 1);
    settle(&b, &mock, 1);
    for block in &b_blocks {
        assert!(matches!(a.accept(block).unwrap().outcome, Accepted::Side));
    }
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.header_seen(&b1), Some(fork + 2));
    assert_eq!(snapshot.header_seen(&b2), Some(fork + 2));
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), Some((b2, 2)));

    // The BTC reorg: X goes, and B's settlement transactions land on the new branch's second block
    let pair =
        ksana_btc::settlement::SettlementPair::decode(b_blocks[1].settlement.as_ref().unwrap())
            .unwrap();
    mock.reorg(3);
    mock.hold(&pair.commit_txid());
    mock.hold(&pair.txid());
    mock.mine(1);
    mock.release(&pair.commit_txid());
    mock.release(&pair.txid());
    mock.mine(2);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(!snapshot.contains(&b_blocks[1].header.btc_anchor));
    assert_eq!(snapshot.header_seen(&b1), Some(fork + 2));
    assert_eq!(a.chain.tip().unwrap().hash, a_tip);
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), Some((b1, 1)));
    rescuer.run_once().unwrap();
    let headers = republished(&mock);
    assert_eq!(headers.len(), 1, "{headers:?}");
    assert_eq!(
        headers[0].iter().map(|h| h.height).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6, 7]
    );

    // A's headers land before block 1 reaches the confirmations: A's chain becomes final, and B's blocks never do
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(a.chain.btc_competitor(&snapshot, 6).unwrap(), None);
    mock.mine(DEVNET.btc_finality_confs);
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (a_tip, 7));
    assert!(!a.chain.is_final(&b1, 1).unwrap());
    assert_eq!(a.ksa(&carol), 0);
}

/// The timestamp must not exceed the anchor BTC block's time plus `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` (D-083): a block
/// exactly at the limit is valid, a block 1 second over is invalid and recorded as an invalid block; the same holds
/// when accepted as a historical block (local time not checked).
#[test]
fn a_block_too_far_ahead_of_its_anchor_time_is_invalid() {
    let (mock, genesis) = setup(&[]);
    let a = Side::new(&mock, &genesis, "m15a3");
    let b = Side::new(&mock, &genesis, "m15b3");
    let good = a.mine();
    let anchor_time = mock
        .header(&good.header.btc_anchor.hash)
        .unwrap()
        .unwrap()
        .header
        .time;
    let consensus = a.chain.consensus();
    let weight = a.chain.weight_of(&good.header.hash()).unwrap().unwrap();
    let with_timestamp = |timestamp: u64| {
        let mut block = good.clone();
        block.header.timestamp = timestamp;
        block.header.nonce = 0;
        assert!(consensus
            .solve(&mut block.header, weight, 0, 1 << 24)
            .unwrap());
        block
    };
    let limit = u64::from(anchor_time) + DEVNET.max_timestamp_ahead_of_anchor_secs;
    let late = with_timestamp(limit + 1);
    match b.accept(&late) {
        Err(NodeError::InvalidBlock(m)) => assert!(m.contains("D-083"), "{m}"),
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert!(b.chain.is_invalid(&late.header.hash()).unwrap());
    // Receiving the same block again: still invalid
    assert!(matches!(b.accept(&late), Err(NodeError::InvalidBlock(_))));
    // Exactly at the limit: valid
    let at_limit = with_timestamp(limit);
    assert!(b.accept(&at_limit).is_ok());
    assert!(!b.chain.is_invalid(&at_limit.header.hash()).unwrap());
}
