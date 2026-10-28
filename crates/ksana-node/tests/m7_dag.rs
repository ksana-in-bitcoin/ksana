//! M7: node behavior of the block DAG (GHOSTDAG, D-063), without the network: several node instances share the same
//! mock BTC chain, each produces blocks, then hands its blocks to the others.
//!
//! - Parallel blocks are merged by a later block: an op included twice executes only once, of conflicting ops with
//!   the same nonce only the one ordered first executes and the rest are skipped; a merged block's token fee goes to
//!   its producer and unlocks once its settlement lands on BTC and enters the settled closure.
//! - Parallel blocks with overlapping forced-inclusion windows: every native transaction executes exactly once, in
//!   BTC order.
//! - When the merge limit is exceeded the lighter side is abandoned, its ops return to the entry pool and are
//!   re-included by later blocks.
//! - After a restart the DAG, the main chain and the acceptance relation are unchanged.

mod common;

use bitcoin::hashes::Hash as _;
use common::mock_node::{native_tx, signed_native};
use common::side::{setup, Side};
use common::{transfer_op, user};
use ksana_consensus::Consensus;
use ksana_execution::keys::{self, LockedFee};
use ksana_node::chain::{Accepted, Chain};
use ksana_primitives::{Block, Entry, Params, DEVNET};

/// Two parallel blocks (same parent): A produces one, B produces one, then each receives the other's. Returns
/// (A's block, B's block).
fn parallel(a: &Side, b: &Side) -> (Block, Block) {
    let a1 = a.mine();
    let b1 = b.mine();
    assert_eq!(a1.header.parents, b1.header.parents);
    a.accept(&b1).unwrap();
    b.accept(&a1).unwrap();
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());
    (a1, b1)
}

/// The main-chain tip (of two parallel blocks with equal blue work, the one with the larger hash) and the other block.
fn ordered(x: &Block, y: &Block) -> (Block, Block) {
    if x.header.hash() > y.header.hash() {
        (x.clone(), y.clone())
    } else {
        (y.clone(), x.clone())
    }
}

/// The block's sponsored fee record, by its acceptance position on the main chain (D-079); `None` when the block has
/// not been accepted by the main chain yet or has no sponsored fee.
fn locked_fee(side: &Side, block: &Block) -> Option<LockedFee> {
    let hash = block.header.hash();
    let main = side.chain.main_chain().unwrap();
    let (height, index) = main.iter().find_map(|(height, entry)| {
        entry
            .accepted()
            .position(|b| b.hash == hash)
            .map(|i| (height, u32::try_from(i).unwrap()))
    })?;
    side.chain
        .with_state(|state, _| {
            state
                .get(&keys::locked_fee(height, index))
                .unwrap()
                .map(|v| borsh::from_slice::<LockedFee>(&v).unwrap())
        })
        .unwrap()
}

fn producer_of(name: &str) -> [u8; 32] {
    user(&format!("m4/{name}/producer")).account
}

/// The same op is included by two parallel blocks, and two different ops of the same account with the same nonce are
/// in the two parallel blocks respectively: the later block merges the other parallel block, the duplicate executes
/// only once, of the conflicting ones only the one on the main chain executes (the other is skipped: no fee charged,
/// nonce unchanged); the two nodes' states agree. The token fees of the ops executed in the merged block are recorded
/// under its own name (going to its producer). After a restart the DAG and the acceptance relation are unchanged.
#[test]
fn parallel_blocks_are_merged_and_each_op_executes_once() {
    let (alice, bob, carol, dave, erin, frank) = (
        user("m7/alice"),
        user("m7/bob"),
        user("m7/carol"),
        user("m7/dave"),
        user("m7/erin"),
        user("m7/frank"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000), (&dave, 1_000)]);
    let a = Side::new(&mock, &genesis, "m7a");
    let b = Side::new(&mock, &genesis, "m7b");

    let duplicate = transfer_op(&alice, 0, 3, bob.account, 10);
    let to_erin = transfer_op(&dave, 0, 2, erin.account, 5);
    let to_frank = transfer_op(&dave, 0, 4, frank.account, 6);
    let extra = transfer_op(&carol, 0, 4, frank.account, 1);
    a.submit(duplicate.clone());
    a.submit(to_erin.clone());
    b.submit(duplicate.clone());
    b.submit(to_frank.clone());
    b.submit(extra.clone());

    let (a1, b1) = parallel(&a, &b);
    let (tip, other) = ordered(&a1, &b1);
    assert_eq!(a.chain.tip().unwrap().hash, tip.header.hash());
    // The other parallel block is a DAG tip but not on the main chain
    let mut tips = a.chain.dag_tips().unwrap();
    tips.sort_unstable();
    let mut expected = vec![a1.header.hash(), b1.header.hash()];
    expected.sort_unstable();
    assert_eq!(tips, expected);
    assert_eq!(a.chain.accepted_height(&other.header.hash()).unwrap(), None);

    // A's next block merges it
    let a2 = a.mine();
    assert_eq!(
        a2.header.parents,
        vec![tip.header.hash(), other.header.hash()]
    );
    let report = b.accept(&a2).unwrap();
    assert!(matches!(report.outcome, Accepted::Extended(_)));
    assert_eq!(report.connected, vec![other.clone(), a2.clone()]);
    for side in [&a, &b] {
        assert_eq!(side.chain.tip().unwrap().hash, a2.header.hash());
        assert_eq!(side.chain.dag_tips().unwrap(), vec![a2.header.hash()]);
        assert_eq!(
            side.chain.accepted_height(&other.header.hash()).unwrap(),
            Some(2)
        );
        // The duplicate executes only once
        assert_eq!(side.ksa(&bob), 10);
        assert_eq!(side.ksa(&alice), 1_000 - 10 - 3);
        assert_eq!(side.nonce(&alice), 1);
        // Of the conflicting ones only the one in the main-chain block executes
        let tip_is_a = tip.header.hash() == a1.header.hash();
        let (erin_got, frank_got, dave_paid) = if tip_is_a {
            (5, 1, 5 + 2)
        } else {
            (0, 6 + 1, 6 + 4)
        };
        assert_eq!(side.ksa(&erin), erin_got);
        assert_eq!(side.ksa(&frank), frank_got);
        assert_eq!(side.ksa(&dave), 1_000 - dave_paid);
        assert_eq!(side.nonce(&dave), 1);
        assert_eq!(side.ksa(&carol), 1_000 - 1 - 4);
        // Op index: the duplicate points to the main-chain block that executed it, the skipped one of the conflict is
        // marked skipped
        let store = side.chain.store();
        let location = store
            .op_location(&duplicate.op_id().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(location.block, tip.header.hash());
        assert!(!location.skipped);
        let loser = if tip_is_a { &to_frank } else { &to_erin };
        let location = store.op_location(&loser.op_id().unwrap()).unwrap().unwrap();
        assert_eq!(location.block, other.header.hash());
        assert!(location.skipped);
        // Sponsored fees are recorded at each block's own acceptance position (D-079): the main-chain block's go to
        // its producer, those executed in the merged block go to the merged block's producer; block rewards are not
        // recorded and are paid out at unlock
        let tip_fee = locked_fee(side, &tip).unwrap();
        assert_eq!(tip_fee.producer, tip.header.producer);
        if tip_is_a {
            assert_eq!(tip_fee.amount, 3 + 2);
            let other_fee = locked_fee(side, &other).unwrap();
            assert_eq!(other_fee.producer, producer_of("m7b"));
            assert_eq!(
                other_fee.amount, 4,
                "only carol's op in B's block was executed"
            );
        } else {
            assert_eq!(tip_fee.amount, 3 + 4 + 4);
            assert_eq!(
                locked_fee(side, &other),
                None,
                "the ops in A's block were all skipped, no sponsored fee"
            );
        }
    }
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());

    // After a restart the DAG, the main chain and the acceptance relation are unchanged
    let tip = a.chain.tip().unwrap();
    let dag_len = a.chain.dag_len().unwrap();
    let Side {
        chain,
        producer,
        dir,
        ..
    } = a;
    drop(producer);
    drop(chain);
    let reopened = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert_eq!(reopened.tip().unwrap(), tip);
    assert_eq!(reopened.dag_len().unwrap(), dag_len);
    assert_eq!(reopened.dag_tips().unwrap(), vec![tip.hash]);
    for block in [&a1, &b1, &a2] {
        assert!(reopened
            .accepted_height(&block.header.hash())
            .unwrap()
            .is_some());
    }
    assert_eq!(reopened.main_chain().unwrap().entries().len(), 3);
}

fn forced_txids(block: &Block) -> Vec<[u8; 32]> {
    block
        .entries
        .iter()
        .filter_map(|e| match e {
            Entry::Forced { btc_txid, .. } => Some(*btc_txid),
            _ => None,
        })
        .collect()
}

/// Forced inclusion (§7.4, D-063): the inclusion windows of two parallel blocks overlap (A's anchor is lower and holds
/// only the first native transaction; B's anchor is higher and holds both), after the merge every native transaction
/// executes exactly once, in BTC order; the window of the block merging them starts from the highest anchor among the
/// parents and includes nothing more.
#[test]
fn overlapping_forced_windows_execute_each_native_tx_once() {
    let (carol, dave) = (user("m7/carol2"), user("m7/dave2"));
    let (mock, genesis) = setup(&[(&carol, 100)]);
    let a = Side::new(&mock, &genesis, "m7a2");
    let b = Side::new(&mock, &genesis, "m7b2");
    let (t1, first) = native_tx(&mock, |input| {
        signed_native(&carol, 0, input, dave.account, 9)
    });
    mock.mine_block(vec![t1.clone()]).unwrap();
    let a1 = a.mine();
    let (t2, second) = native_tx(&mock, |input| {
        signed_native(&carol, 1, input, dave.account, 4)
    });
    mock.mine_block(vec![t2.clone()]).unwrap();
    let b1 = b.mine();
    let (id1, id2) = (
        t1.compute_txid().to_byte_array(),
        t2.compute_txid().to_byte_array(),
    );
    assert_eq!(forced_txids(&a1), vec![id1]);
    assert_eq!(forced_txids(&b1), vec![id1, id2]);
    assert!(a1.header.btc_anchor.height < b1.header.btc_anchor.height);

    a.accept(&b1).unwrap();
    b.accept(&a1).unwrap();
    let a2 = a.mine();
    assert_eq!(a2.header.parents.len(), 2);
    assert!(
        forced_txids(&a2).is_empty(),
        "the window starts from the highest anchor among the parents"
    );
    b.accept(&a2).unwrap();
    for side in [&a, &b] {
        assert_eq!(side.chain.tip().unwrap().hash, a2.header.hash());
        assert_eq!(side.ksa(&dave), 9 + 4);
        assert_eq!(side.nonce(&carol), 2);
        for op in [&first, &second] {
            let location = side
                .chain
                .store()
                .op_location(&op.op_id().unwrap())
                .unwrap()
                .unwrap();
            assert!(!location.skipped);
        }
    }
}

/// Exceeding the merge limit (here the merge set limit is set to 4, including the selected parent): the lighter side's
/// 4 blocks cannot be merged and are abandoned; their ops return to the entry pool when that side's node switches its
/// main chain and are re-included by later blocks.
#[test]
fn exceeding_the_merge_limit_abandons_the_lighter_side() {
    let params = Params {
        max_mergeset_size: 4,
        ..DEVNET
    };
    let (carol, dave) = (user("m7/carol3"), user("m7/dave3"));
    let (mock, genesis) = setup(&[(&carol, 1_000)]);
    let a = Side::with_params(&mock, &genesis, "m7a3", params.clone());
    let b = Side::with_params(&mock, &genesis, "m7b3", params);
    let op = transfer_op(&carol, 0, 2, dave.account, 6);
    b.submit(op.clone());
    let b_blocks: Vec<Block> = (0..4).map(|_| b.mine()).collect();
    let a_blocks: Vec<Block> = (0..6).map(|_| a.mine()).collect();
    assert_eq!(b.ksa(&dave), 6);

    // B switches to A's chain: its own blocks are disconnected and the op returns to the entry pool
    for block in &a_blocks {
        b.accept(block).unwrap();
    }
    assert_eq!(b.chain.tip().unwrap(), a.chain.tip().unwrap());
    assert_eq!(b.ksa(&dave), 0);
    assert!(b.pending(&op));
    // B's next block cannot merge its former branch (merge set 4 + 1 > 4), has only A's tip as parent and re-includes
    // the op
    let next = b.mine();
    assert_eq!(next.header.parents, vec![a_blocks[5].header.hash()]);
    assert!(next
        .entries
        .iter()
        .any(|e| matches!(e, Entry::Sponsored { op: o } if *o == op)));
    for block in b_blocks.iter().chain([&next]) {
        a.accept(block).unwrap();
    }
    let after = a.mine();
    assert_eq!(after.header.parents, vec![next.header.hash()]);
    b.accept(&after).unwrap();
    for side in [&a, &b] {
        assert_eq!(side.chain.tip().unwrap().hash, after.header.hash());
        assert_eq!(side.ksa(&dave), 6);
        for block in &b_blocks {
            assert_eq!(
                side.chain.accepted_height(&block.header.hash()).unwrap(),
                None
            );
        }
    }
}

/// A merged block's token fee (D-063): recorded under the merged block's name and going to its producer; it unlocks
/// once its settlement lands on BTC and the main-chain block accepting it enters the settled prefix.
#[test]
fn a_merged_blocks_fee_unlocks_for_its_producer() {
    let (alice, bob, carol, dave) = (
        user("m7/alice4"),
        user("m7/bob4"),
        user("m7/carol4"),
        user("m7/dave4"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, "m7a4");
    let b = Side::new(&mock, &genesis, "m7b4");
    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    b.submit(transfer_op(&carol, 0, 4, dave.account, 1));
    let (a1, b1) = parallel(&a, &b);
    let (tip, other) = ordered(&a1, &b1);
    let merging = a.mine();
    assert_eq!(merging.header.parents.len(), 2);
    assert!(a1.settlement.is_some() && b1.settlement.is_some());
    let other_fee = locked_fee(&a, &other).unwrap();
    assert_eq!(other_fee.producer, other.header.producer);
    // The record holds only the sponsored fee; the block reward of a blue merged block is paid out at unlock (D-074,
    // D-079)
    let fee = if other.header.hash() == a1.header.hash() {
        3
    } else {
        4
    };
    assert_eq!(other_fee.amount, fee);

    // The merging block itself is an empty block: the payload of the next block with a settlement transaction carries
    // its header
    a.submit(transfer_op(&alice, 1, 3, bob.account, 1));
    let carrier = a.mine();
    assert!(carrier.settlement.is_some());
    for _ in 0..DEVNET.settlement_depth {
        a.mine();
    }
    // All three settlements have reached SETTLEMENT_DEPTH; broadcast and confirm them
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    for block in [&tip, &other, &carrier] {
        let snapshot = a.chain.btc().snapshot().unwrap();
        assert!(
            snapshot.settlement_of(&block.header.hash()).is_some(),
            "the settlement of block {} is confirmed",
            block.header.height
        );
    }
    // Cumulative unlock (D-074, D-079): the unlock entry names the last settled position, the merged block unlocks
    // with it, the record is deleted and the reward paid out
    let before = a.balance(&other.header.producer);
    let mut unlocked = false;
    for _ in 0..10 {
        a.mine();
        if locked_fee(&a, &other).is_none() {
            unlocked = true;
            break;
        }
    }
    assert!(unlocked, "the merged block's fee and reward are unlocked");
    assert!(a.balance(&other.header.producer) >= before + other_fee.amount + DEVNET.block_reward);
}

/// The rules of header v2 and the DAG (D-063); violating any one of them makes the block invalid and remembered:
/// `parents[0]` is not the selected parent, the other parents are not ascending, parents are ancestors of each other,
/// duplicate parents, too many parents, the height is not the selected parent's plus one, `bits` does not follow
/// `daa_score`, the merge set is too large.
#[test]
fn invalid_dag_headers_are_rejected() {
    let params = Params {
        max_block_parents: 3,
        max_mergeset_size: 3,
        ..DEVNET
    };
    let (mock, genesis) = setup(&[]);
    let side = |name: &str| Side::with_params(&mock, &genesis, name, params.clone());
    let (a, b, c, d) = (side("m7a5"), side("m7b5"), side("m7c5"), side("m7d5"));
    // Three parallel blocks (all with the genesis block as parent); A's next block merges the other two (merge set
    // 3 ≤ 3)
    let parallel: Vec<Block> = [&a, &b, &c].iter().map(|s| s.mine()).collect();
    for block in &parallel[1..] {
        a.accept(block).unwrap();
    }
    let good = a.mine();
    assert_eq!(good.header.parents.len(), 3);
    for block in &parallel {
        d.accept(block).unwrap();
    }

    let consensus = a.chain.consensus();
    // good is an empty block, and its weight follows the selected parent's empty-block streak (D-090); none of the
    // tamperings changes the selected parent's streak (the parallel blocks all have streak 1)
    let good_weight = a.chain.weight_of(&good.header.hash()).unwrap().unwrap();
    let with = |f: &dyn Fn(&mut Block)| {
        let mut block = good.clone();
        f(&mut block);
        block.header.nonce = 0;
        assert!(consensus
            .solve(&mut block.header, good_weight, 0, 1 << 24)
            .unwrap());
        block
    };
    let genesis_hash = a.chain.genesis().hash();
    // Another parallel block w: used as the 4th parent
    let w = side("m7e5").mine();
    d.accept(&w).unwrap();
    let cases: Vec<(&str, Block, &str)> = vec![
        (
            "parents[0] is not the selected parent",
            with(&|blk| blk.header.parents.swap(0, 1)),
            "selected parent",
        ),
        (
            "the other parents are not ascending",
            with(&|blk| blk.header.parents[1..].reverse()),
            "ascending",
        ),
        (
            "parents are ancestors of each other",
            with(&|blk| {
                blk.header.parents[2] = genesis_hash;
                blk.header.parents[1..].sort_unstable();
            }),
            "past",
        ),
        (
            "duplicate parents",
            with(&|blk| {
                let first = blk.header.parents[1];
                blk.header.parents.insert(1, first);
                blk.header.parents.truncate(3);
            }),
            "duplicate",
        ),
        (
            "too many parents",
            with(&|blk| {
                blk.header.parents.push(w.header.hash());
                blk.header.parents[1..].sort_unstable();
            }),
            "parents",
        ),
        (
            "height is not the selected parent's plus one",
            with(&|blk| blk.header.height += 1),
            "height",
        ),
        (
            "bits does not follow daa_score",
            with(&|blk| blk.header.bits -= 1),
            "bits",
        ),
    ];
    for (what, block, reason) in &cases {
        match d.accept(block) {
            Err(ksana_node::NodeError::InvalidBlock(m)) => {
                assert!(m.contains(reason), "{what}: {m}")
            }
            other => panic!("{what}: {other:?}"),
        }
        assert!(d.chain.is_invalid(&block.header.hash()).unwrap(), "{what}");
    }

    // Merge set too large: two chains of 3 blocks each; the block has one chain's tip as selected parent and the
    // other's tip as second parent, merge set 1 + 3 > 3
    let (u, v) = (side("m7f5"), side("m7g5"));
    let u_chain: Vec<Block> = (0..4).map(|_| u.mine()).collect();
    let v_chain: Vec<Block> = (0..3).map(|_| v.mine()).collect();
    for block in u_chain[..3].iter().chain(&v_chain) {
        d.accept(block).unwrap();
    }
    let (u3, v3) = (u_chain[2].header.hash(), v_chain[2].header.hash());
    let mut big = u_chain[3].clone();
    big.header.parents = vec![u3.max(v3), u3.min(v3)];
    big.header.nonce = 0;
    // Both chains are 3 empty blocks, so whichever is the selected parent the streak is 3: the weight equals the
    // original block's
    let big_weight = u
        .chain
        .weight_of(&u_chain[3].header.hash())
        .unwrap()
        .unwrap();
    assert!(consensus
        .solve(&mut big.header, big_weight, 0, 1 << 24)
        .unwrap());
    match d.accept(&big) {
        Err(ksana_node::NodeError::InvalidBlock(m)) => assert!(m.contains("merge set"), "{m}"),
        other => panic!("merge set too large: {other:?}"),
    }
    assert!(d.chain.is_invalid(&big.header.hash()).unwrap());

    // A valid block connects as usual
    assert!(d.accept(&good).is_ok());
    assert!(d.chain.contains(&good.header.hash()).unwrap());
}
