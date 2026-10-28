//! M4: main-chain selection and reorg of the block DAG (D-041, D-063), without the network: two node instances share
//! the same mock BTC chain, each produces blocks, then hands its blocks to the other.
//!
//! - The branch with more chain work (its tip's blue work plus the tip's own work, D-100) becomes the main chain: the
//!   state rolls back to the fork point and re-executes along the new selected chain; the ops of disconnected blocks
//!   are no longer effective and their op index is deleted. One heavy empty block outweighs a branch with more blue
//!   work but less work in all.
//! - A block whose state root does not match cannot be a main-chain block (D-063): when the main-chain switch fails,
//!   the original main chain and state are restored; it stays in the DAG and is merged later.
//! - A block with an invalid header and its descendants are rejected.
//! - Orphan blocks connect once their parent arrives; crash recovery when the state store lags behind the block store.

mod common;

use common::side::{setup, Side};
use common::{transfer_op, user};
use ksana_consensus::Consensus;
use ksana_node::chain::{Accepted, Chain};
use ksana_node::NodeError;
use ksana_primitives::{Block, DEVNET};

#[test]
fn heavier_branch_wins_and_state_rolls_back() {
    let (alice, bob, carol) = (user("m4/alice"), user("m4/bob"), user("m4/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "a");
    let b = Side::new(&mock, &genesis, "b");
    assert_eq!(a.chain.genesis().hash(), b.chain.genesis().hash());

    // The same nonce is spent once on each branch: to bob on A, to carol on B
    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let a_blocks: Vec<Block> = (0..2).map(|_| a.mine()).collect();
    let b_blocks: Vec<Block> = (0..4).map(|_| b.mine()).collect();
    assert_eq!(a.ksa(&bob), 10);
    assert_eq!(b.ksa(&carol), 20);

    // A receives B's branch: reorg once its work exceeds A's main chain
    let mut reorganized = None;
    for block in &b_blocks {
        let report = a.accept(block).unwrap();
        match report.outcome {
            Accepted::Side => {}
            Accepted::Reorganized(tip) => {
                assert_eq!(
                    report.disconnected, a_blocks,
                    "A's own two blocks are disconnected"
                );
                reorganized = Some(tip);
            }
            Accepted::Extended(_) => {
                assert!(reorganized.is_some(), "extended in order after the reorg")
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert!(reorganized.is_some());
    let (ta, tb) = (a.chain.tip().unwrap(), b.chain.tip().unwrap());
    assert_eq!(ta.hash, tb.hash);
    assert_eq!(ta.chain_work, tb.chain_work);
    assert_eq!(ta.header.state_root, tb.header.state_root);
    // State rollback: the transfer on A's branch disappears, the one on B's branch takes effect; the nonce is used
    // once
    assert_eq!(a.ksa(&bob), 0);
    assert_eq!(a.ksa(&carol), 20);
    assert_eq!(a.ksa(&alice), 1_000 - 20 - 4);
    assert_eq!(a.nonce(&alice), 1);
    for (height, block) in (1u64..).zip(&b_blocks) {
        assert_eq!(
            a.chain.store().hash_at(height).unwrap(),
            Some(block.header.hash())
        );
    }
    // The ops of disconnected blocks no longer have a main-chain location
    let bob_op = transfer_op(&alice, 0, 3, bob.account, 10).op_id().unwrap();
    assert_eq!(a.chain.store().op_location(&bob_op).unwrap(), None);
    // The other direction: to B, A's former blocks are only a side chain with less work
    for block in &a_blocks {
        let report = b.accept(block).unwrap();
        assert!(
            matches!(report.outcome, Accepted::Side),
            "{:?}",
            report.outcome
        );
    }
    assert_eq!(b.chain.tip().unwrap().hash, tb.hash);
    // After a restart the state matches the tip
    drop(a.producer);
    let dir = a.dir;
    drop(a.chain);
    let reopened = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert_eq!(reopened.tip().unwrap().hash, tb.hash);
}

/// D-100: the fork choice counts the tip's own work. An empty block on the genesis block (weight at least
/// `EMPTY_BLOCK_DIFFICULTY`, D-084) against two non-empty blocks: the two have more blue work (the first one's work
/// counts in the second's), the empty block more chain work. A node holding both branches takes the empty block, and
/// the ops of the branch it leaves go back to its entry pool.
#[test]
fn a_heavy_tip_outweighs_a_branch_with_more_blue_work() {
    let (alice, bob) = (user("m4/heavy/alice"), user("m4/heavy/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "heavy/a");
    let b = Side::new(&mock, &genesis, "heavy/b");
    let heavy = a.mine();
    assert!(heavy.entries.is_empty());
    let ops: Vec<_> = (0..2)
        .map(|nonce| transfer_op(&alice, nonce, 1, bob.account, 1))
        .collect();
    let light: Vec<Block> = ops
        .iter()
        .map(|op| {
            b.submit(op.clone());
            b.mine()
        })
        .collect();
    assert!(light.iter().all(|block| block.entries.len() == 1));
    assert_eq!(b.ksa(&bob), 2);

    let report = b.accept(&heavy).unwrap();
    assert!(
        matches!(report.outcome, Accepted::Reorganized(_)),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.disconnected, light);
    let (heavy_hash, light_tip) = (heavy.header.hash(), light[1].header.hash());
    let data = |hash| b.chain.ghostdag(&hash).unwrap().unwrap();
    assert!(data(light_tip).blue_work > data(heavy_hash).blue_work);
    let tip = b.chain.tip().unwrap();
    assert_eq!(tip.hash, heavy_hash);
    assert!(tip.chain_work > data(light_tip).blue_work);
    assert_eq!(b.ksa(&bob), 0);
    assert!(ops.iter().all(|op| b.pending(op)));
    // The node that made the empty block keeps it
    for block in &light {
        assert!(matches!(a.accept(block).unwrap().outcome, Accepted::Side));
    }
    assert_eq!(a.chain.tip().unwrap().hash, heavy_hash);
}

/// A block with a valid header whose state root does not match after re-execution as a main-chain block (D-063): the
/// main-chain switch to it fails and the original main chain and state are restored; it cannot be a main-chain block,
/// nor can its descendants whose selected chain passes through it, but they all stay in the DAG (not invalid blocks)
/// and later blocks merge them as usual (merged blocks are not checked for their state root).
#[test]
fn a_block_with_a_wrong_state_root_is_disqualified_but_stays_mergeable() {
    let (alice, bob) = (user("m4/alice2"), user("m4/bob2"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "a2");
    let b = Side::new(&mock, &genesis, "b2");
    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    let a_block = a.mine();
    let blocks: Vec<Block> = (0..3).map(|_| b.mine()).collect();

    // Tamper with the state root of B's block 2 (re-solving the PoW) and attach block 3 after it. The parents are
    // unchanged and all are empty blocks: the weight (by the selected parent's empty-block streak, D-090) is the same
    // as the original blocks'
    let consensus = b.chain.consensus();
    let weight_of = |block: &Block| b.chain.weight_of(&block.header.hash()).unwrap().unwrap();
    let mut bad = blocks[1].clone();
    bad.header.state_root[0] ^= 1;
    bad.header.nonce = 0;
    assert!(consensus
        .solve(&mut bad.header, weight_of(&blocks[1]), 0, 1 << 24)
        .unwrap());
    let mut child = blocks[2].clone();
    child.header.parents = vec![bad.header.hash()];
    child.header.nonce = 0;
    assert!(consensus
        .solve(&mut child.header, weight_of(&blocks[2]), 0, 1 << 24)
        .unwrap());
    let (bad_hash, child_hash) = (bad.header.hash(), child.header.hash());

    // As soon as bad, and then child, with more chain work arrive, a main-chain switch is attempted and fails when
    // executing bad
    for block in [&blocks[0], &bad, &child] {
        a.accept(block).unwrap();
    }
    assert!(a.chain.is_disqualified(&bad_hash).unwrap());
    assert!(!a.chain.is_invalid(&bad_hash).unwrap());
    assert!(a.chain.contains(&child_hash).unwrap());
    assert_eq!(a.chain.accepted_height(&bad_hash).unwrap(), None);
    assert_eq!(a.chain.accepted_height(&child_hash).unwrap(), None);
    // The main chain is B's height-1 block: the same blue work as A's block, but as an empty block it has more work of
    // its own (D-100); the state matches it
    let (a_hash, b_hash) = (a_block.header.hash(), blocks[0].header.hash());
    let blue_work = |hash| a.chain.ghostdag(&hash).unwrap().unwrap().blue_work;
    assert_eq!(blue_work(a_hash), blue_work(b_hash));
    assert!(a.chain.weight_of(&b_hash).unwrap() > a.chain.weight_of(&a_hash).unwrap());
    assert_eq!(a.chain.tip().unwrap().hash, b_hash);
    assert_eq!(a.ksa(&bob), 0);

    // A keeps producing: later blocks merge the other side's blocks, including bad and child, which cannot be
    // main-chain blocks
    let mut merged = false;
    for _ in 0..6 {
        a.mine();
        if a.chain.accepted_height(&child_hash).unwrap().is_some() {
            merged = true;
            break;
        }
    }
    assert!(merged, "child is merged");
    for hash in [a_block.header.hash(), blocks[0].header.hash(), bad_hash] {
        assert!(a.chain.accepted_height(&hash).unwrap().is_some());
    }
    assert!(a.chain.is_disqualified(&bad_hash).unwrap());
    assert_eq!(a.ksa(&bob), 10);
    assert_eq!(a.ksa(&alice), 1_000 - 10 - 3);
    assert_eq!(a.nonce(&alice), 1);
}

/// A block with an invalid header (here mismatching `bits`) is rejected and remembered, and does not enter the DAG;
/// its descendants are rejected as well (D-041, D-063).
#[test]
fn an_invalid_block_and_its_descendants_are_rejected() {
    let (mock, genesis) = setup(&[]);
    let a = Side::new(&mock, &genesis, "a5");
    let b = Side::new(&mock, &genesis, "b5");
    let blocks: Vec<Block> = (0..2).map(|_| b.mine()).collect();

    let consensus = b.chain.consensus();
    let weight_of = |block: &Block| b.chain.weight_of(&block.header.hash()).unwrap().unwrap();
    let mut bad = blocks[0].clone();
    bad.header.bits -= 1;
    bad.header.nonce = 0;
    assert!(consensus
        .solve(&mut bad.header, weight_of(&blocks[0]), 0, 1 << 24)
        .unwrap());
    let mut child = blocks[1].clone();
    child.header.parents = vec![bad.header.hash()];
    child.header.nonce = 0;
    assert!(consensus
        .solve(&mut child.header, weight_of(&blocks[1]), 0, 1 << 24)
        .unwrap());

    assert!(matches!(a.accept(&bad), Err(NodeError::InvalidBlock(_))));
    assert!(a.chain.is_invalid(&bad.header.hash()).unwrap());
    assert!(!a.chain.contains(&bad.header.hash()).unwrap());
    assert!(matches!(a.accept(&child), Err(NodeError::InvalidBlock(_))));
    assert!(a.chain.is_invalid(&child.header.hash()).unwrap());
    // Valid blocks connect as usual
    for block in &blocks {
        a.accept(block).unwrap();
    }
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());
}

#[test]
fn orphans_connect_when_their_parent_arrives() {
    let (mock, genesis) = setup(&[]);
    let a = Side::new(&mock, &genesis, "a3");
    let replica = Side::new(&mock, &genesis, "r3");
    let blocks: Vec<Block> = (0..4).map(|_| a.mine()).collect();
    for block in blocks[1..].iter().rev() {
        let report = replica.accept(block).unwrap();
        assert!(matches!(report.outcome, Accepted::Orphan));
    }
    assert_eq!(replica.chain.orphan_count().unwrap(), 3);
    let report = replica.accept(&blocks[0]).unwrap();
    assert!(matches!(report.outcome, Accepted::Extended(_)));
    assert_eq!(
        report.connected, blocks,
        "orphans connect in order once the parent arrives"
    );
    assert_eq!(replica.chain.orphan_count().unwrap(), 0);
    assert_eq!(replica.chain.tip().unwrap(), a.chain.tip().unwrap());
    // A duplicate block
    assert!(matches!(
        replica.accept(&blocks[2]).unwrap().outcome,
        Accepted::Duplicate
    ));
}

/// Crash recovery (D-041): when the state store lags behind the block store (e.g. a power loss lost the state store's
/// last writes), roll back to the version consistent with the main chain and, after BTC sync, re-execute the later
/// blocks, recovering the original tip.
#[test]
fn state_behind_the_block_store_is_recovered() {
    let (alice, bob) = (user("m4/alice4"), user("m4/bob4"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "a4");
    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    for _ in 0..4 {
        a.mine();
    }
    let tip = a.chain.tip().unwrap();
    assert_eq!(tip.header.height, 4);
    let Side {
        chain,
        producer,
        dir,
        ..
    } = a;
    drop(producer);
    drop(chain);
    // Simulate the state store losing its last two versions
    {
        let mut state = ksana_state::StateDb::open(&dir.path().join("state")).unwrap();
        state.rollback_to(2).unwrap();
    }
    let chain = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert_eq!(
        chain.tip().unwrap().header.height,
        2,
        "first fall back to a consistent height"
    );
    while chain.sync_btc().unwrap() {}
    chain.resume().unwrap();
    let recovered = chain.tip().unwrap();
    assert_eq!(recovered, tip);
    // Opening again needs no recovery
    drop(chain);
    let chain = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
    chain.resume().unwrap();
}

/// A node joining late or catching up after downtime: the settlement on BTC confirmed before it learned of that
/// fast-lane block (it did not know the block yet when scanning BTC). Once it learns of the block, the settlement must
/// still be counted, otherwise a valid block holding that block's unlock entry would be misjudged invalid (D-035,
/// D-038).
#[test]
fn late_joiner_counts_settlements_confirmed_before_it_knew_the_block() {
    use ksana_primitives::Entry;

    let (alice, bob) = (user("m4/alice6"), user("m4/bob6"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "a6");
    a.submit(transfer_op(&alice, 0, 5, bob.account, 10));
    let mut blocks: Vec<Block> = (0..=DEVNET.settlement_depth).map(|_| a.mine()).collect();
    assert!(
        blocks[0].settlement.is_some(),
        "the path-B block carries a settlement transaction"
    );
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    let mut unlocked = false;
    for _ in 0..10 {
        let block = a.mine();
        unlocked = block
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Unlock { .. }));
        blocks.push(block);
        if unlocked {
            break;
        }
    }
    assert!(unlocked, "A wrote an unlock entry");

    // R starts only after the settlement confirmed: it did not know that block yet when scanning BTC
    let r = Side::new(&mock, &genesis, "r6");
    for block in &blocks {
        r.accept(block).unwrap();
    }
    assert_eq!(r.chain.tip().unwrap(), a.chain.tip().unwrap());
    assert_eq!(r.ksa(&bob), 10);
}
