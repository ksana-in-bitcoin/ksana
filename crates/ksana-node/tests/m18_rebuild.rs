//! M18 cleanup of implementation limitations (D-093): when the state at the fork point has been pruned (deeper than
//! `REORG_WINDOW`), the node rebuilds the state from the genesis block; clearing the data directory is no longer
//! needed.
//!
//! - A forced switch to a BTC-finalized chain (D-082) is not bound by `REORG_WINDOW`. The states from the finalized tip
//!   on are kept (D-101), so the fork point's state is gone only after a BTC reorg undid finality decisions: the node
//!   then retreats its main chain to the finalized tip, clearing the state and re-executing the main chain up to it,
//!   judges the next block there and attaches the finalized chain; a later restart recovers as usual.
//! - An ordinary switch by work is still bound by `REORG_WINDOW` (§6.4).
//! - When crash recovery finds no state version consistent with the main chain (state store lost or belonging to
//!   another chain), it likewise rebuilds from the genesis block, and the recovery after BTC sync re-executes the main
//!   chain.

mod common;

use common::side::{setup, Side};
use common::{transfer_op, user};

use bitcoin::hashes::Hash as _;
use ksana_btc::mock::MockBtc;
use ksana_node::chain::{Accepted, Chain};
use ksana_primitives::{Block, Params, DEVNET};
use ksana_state::{StateDb, WriteSet};

/// Keep only 2 versions of rollback-able state: a fork point slightly deeper is already pruned.
fn small_window() -> Params {
    let mut params = DEVNET.clone();
    params.reorg_window = 2;
    params
}

/// Broadcasts the settlement transactions of blocks that have reached `SETTLEMENT_DEPTH` and gives them `confs`
/// confirmations on BTC.
fn settle(side: &Side, mock: &MockBtc, confs: u32) {
    side.sync();
    side.chain.process_settlements().unwrap();
    mock.mine(confs);
    side.sync();
}

fn txid(tx: &bitcoin::Transaction) -> [u8; 32] {
    tx.compute_txid().to_byte_array()
}

fn oldest_version(side: &Side) -> Option<u64> {
    side.chain
        .with_state(|state, _| state.oldest_version())
        .unwrap()
}

/// A's block 1 is final and A has produced two more blocks since, so it keeps the states from height 1 on only (the
/// finalized tip's, D-101, and the last REORG_WINDOW = 2). B produced a heavier chain from the genesis block in
/// isolation, which A holds as a side chain. A BTC reorg deeper than BTC_FINALITY_CONFS removes A's settlement (beyond
/// the assumption of D-082) and B settles its own chain into the new branch: B's block 1 is to become final once it is
/// judged valid on top of the genesis block (D-101), whose state A discarded. A retreats its main chain to the genesis
/// block, clearing the state and re-running genesis (D-093), judges B's block 1 valid, and the forced switch to B's chain
/// (depth 5 > REORG_WINDOW 2) follows; afterwards extend and restart as usual, and finality goes on: A's next block does
/// not merge A's former blocks anchored on the BTC blocks the reorg orphaned (D-101), only the one below them (D-104
/// item k), and finality passes it once the data of that block's chain lands again.
#[test]
fn a_btc_reorg_undoing_finality_below_the_states_kept_rebuilds_the_state() {
    let (alice, bob, carol) = (user("m18/alice1"), user("m18/bob1"), user("m18/carol1"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::with_params(&mock, &genesis, "m18a1", small_window());
    let b = Side::with_params(&mock, &genesis, "m18b1", small_window());
    let fork_height = mock.tip_height();
    let confs = DEVNET.btc_finality_confs;

    // A's block 1 transfers to bob, then two more blocks so it reaches SETTLEMENT_DEPTH; B, in isolation, spends the
    // same nonce to carol on a heavier chain
    let op_to_bob = transfer_op(&alice, 0, 3, bob.account, 10);
    a.submit(op_to_bob.clone());
    let mut a_blocks: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let b_blocks: Vec<Block> = (0..6).map(|_| b.mine()).collect();
    let (a1, b1, b6) = (
        a_blocks[0].header.hash(),
        b_blocks[0].header.hash(),
        b_blocks[5].header.hash(),
    );

    // A's settlement lands and reaches the confirmations: block 1 is final; after two more blocks the states below the
    // finalized tip are discarded
    settle(&a, &mock, confs);
    assert_eq!(a.chain.finalized().unwrap(), (a1, 1));
    a_blocks.extend((0..2).map(|_| a.mine()));
    assert_eq!(a.chain.tip().unwrap().header.height, 5);
    assert_eq!(
        oldest_version(&a),
        Some(1),
        "the states from the finalized tip on are kept, within REORG_WINDOW of the tip"
    );
    for block in &b_blocks {
        assert!(matches!(a.accept(block).unwrap().outcome, Accepted::Side));
    }

    // BTC reorgs out the block holding A's settlement and all the confirmations after it; A's settlement transactions,
    // back in the mempool, are held and never land again. A's decision falls back to the genesis block, and B's chain
    // is still only a side chain to A (a switch by work of depth 5 exceeds REORG_WINDOW)
    let removed = mock.reorg(mock.tip_height() - fork_height);
    assert_eq!(removed.len() as u32, confs);
    let held: Vec<[u8; 32]> = mock.mempool().iter().map(txid).collect();
    for tx in &held {
        mock.hold(tx);
    }
    a.sync();
    assert_eq!(a.chain.finalized().unwrap().1, 0);
    assert_eq!(a.chain.tip().unwrap().header.height, 5);

    // B settles its chain into the new branch and its block 1 becomes final on B. A must judge it on top of the
    // genesis block: it retreats its main chain there, rebuilding the state, and then follows the finalized chain
    settle(&b, &mock, confs);
    assert_eq!(b.chain.finalized().unwrap(), (b1, 1));
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (b1, 1));
    assert_eq!(a.chain.tip().unwrap().hash, b6);
    for block in &a_blocks {
        assert_eq!(a.chain.accepted_height(&block.header.hash()).unwrap(), None);
    }
    assert_eq!((a.ksa(&bob), a.ksa(&carol)), (0, 20));
    assert_eq!(a.nonce(&alice), 1);
    // A's former transfer has its nonce already used on the new main chain and does not return to the entry pool
    assert!(!a.pending(&op_to_bob));

    // Afterwards extend as usual: B's next block extends A's chain, and A's next block extends B's. A's next block does
    // not merge the tip of A's former chain: blocks 4 and 5 anchor BTC blocks the reorg orphaned, and no block merging
    // them could ever become BTC-final (D-101). It merges block 3 below them instead, whose only child is block 4
    // (D-104 item k)
    let b_next = b.mine();
    assert!(matches!(
        a.accept(&b_next).unwrap().outcome,
        Accepted::Extended(_)
    ));
    let a_next = a.mine();
    assert_eq!(
        a_next.header.parents,
        vec![b_next.header.hash(), a_blocks[2].header.hash()]
    );
    for block in &a_blocks {
        assert!(matches!(b.accept(block).unwrap().outcome, Accepted::Side));
    }
    assert!(matches!(
        b.accept(&a_next).unwrap().outcome,
        Accepted::Extended(_)
    ));
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());

    // After the rebuild the block store and the state agree: a restart needs no recovery
    let tip = a.chain.tip().unwrap();
    let Side {
        chain,
        producer,
        dir,
        ..
    } = a;
    drop(producer);
    drop(chain);
    let chain = Chain::open(dir.path(), small_window(), &genesis, mock.clone()).unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
    while chain.sync_btc().unwrap() {}
    chain.resume().unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
    assert_eq!(
        chain.finalized().unwrap(),
        (b1, 1),
        "the header DAG and finality are in memory; rescanning BTC yields the same decision"
    );

    // B lands its chain: finality stops below A's next block, as the data of blocks 1 to 3 it merges, taken off BTC by
    // the reorg, has not landed again
    for _ in 0..4 {
        b.mine();
        b.mine();
        settle(&b, &mock, confs + 1);
    }
    assert!(b.chain.finalized().unwrap().1 < a_next.header.height);
    // Once A's settlement transactions land again (or a rescue settlement carries the same data, D-045), finality goes
    // past it (merging blocks 4 and 5 would have excluded it, and every block on top of it, for good, D-101 item c)
    for tx in &held {
        mock.release(tx);
    }
    for _ in 0..4 {
        b.mine();
        b.mine();
        settle(&b, &mock, confs + 1);
    }
    assert!(b.chain.finalized().unwrap().1 > a_next.header.height);
}

/// Without finality involved, a heavier chain deeper than `REORG_WINDOW` is still rejected (§6.4): B keeps its own
/// main chain and A's blocks are only a side chain.
#[test]
fn a_heavier_chain_deeper_than_the_reorg_window_is_still_rejected() {
    let alice = user("m18/alice2");
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::with_params(&mock, &genesis, "m18a2", small_window());
    let b = Side::with_params(&mock, &genesis, "m18b2", small_window());

    let b_blocks: Vec<Block> = (0..4).map(|_| b.mine()).collect();
    let a_blocks: Vec<Block> = (0..6).map(|_| a.mine()).collect();
    assert!(a.chain.tip().unwrap().chain_work > b.chain.tip().unwrap().chain_work);
    for block in &a_blocks {
        assert!(matches!(b.accept(block).unwrap().outcome, Accepted::Side));
    }
    assert_eq!(b.chain.tip().unwrap().hash, b_blocks[3].header.hash());
    assert_eq!(
        oldest_version(&b),
        Some(0),
        "the finalized tip is still the genesis block, whose state is kept (D-101)"
    );
}

/// Crash recovery finds no state version consistent with the main chain: the state store belongs to another chain
/// (even the genesis root differs) or is lost entirely. In both cases the state is rebuilt from the genesis block, and
/// after BTC sync the original main chain is re-executed, recovering the original tip.
#[test]
fn recovery_without_a_consistent_state_version_rebuilds_from_genesis() {
    let (alice, bob) = (user("m18/alice3"), user("m18/bob3"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m18a3");
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
    let state_dir = dir.path().join("state");
    let balance = |chain: &Chain| {
        chain
            .with_state(|state, _| {
                state
                    .get(&ksana_execution::keys::balance(
                        &ksana_primitives::KSA_ASSET_ID,
                        &bob.account,
                    ))
                    .unwrap()
                    .map(|v| borsh::from_slice::<u128>(&v).unwrap())
                    .unwrap_or(0)
            })
            .unwrap()
    };

    // 1. The state store is replaced by another chain's: no version's root matches a main-chain block
    std::fs::remove_dir_all(&state_dir).unwrap();
    {
        let mut state = StateDb::open(&state_dir).unwrap();
        let writes: WriteSet = [(b"x".to_vec(), Some(b"y".to_vec()))].into_iter().collect();
        state.commit(0, &writes).unwrap();
        state.commit(1, &WriteSet::new()).unwrap();
    }
    let chain = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert_eq!(
        chain.tip().unwrap().header.height,
        0,
        "fell back to the genesis block"
    );
    assert_eq!(balance(&chain), 0);
    while chain.sync_btc().unwrap() {}
    chain.resume().unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
    assert_eq!(balance(&chain), 10);
    drop(chain);

    // 2. The state store is lost entirely
    std::fs::remove_dir_all(&state_dir).unwrap();
    let chain = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert_eq!(chain.tip().unwrap().header.height, 0);
    while chain.sync_btc().unwrap() {}
    chain.resume().unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
    assert_eq!(balance(&chain), 10);
    // Opening again needs no recovery
    drop(chain);
    let chain = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
    chain.resume().unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
}
