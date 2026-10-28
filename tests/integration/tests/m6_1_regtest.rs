//! M6.1 regtest acceptance (`tests/integration/run.sh`): a BTC reorg deeper than `ANCHOR_DEPTH` (0 on devnet) takes
//! the blocks anchored on the orphaned BTC blocks off the main chain (D-056, D-104).
//!
//! - Once the node has produced a block anchored on BTC tip X, a path B op is submitted: its block anchors X too, and
//!   its settlement transactions are broadcast; then X is reorged out. The node retreats below the blocks anchored on
//!   X at once: the op comes back into the entry pool and a block on the new branch includes it again. The old
//!   settlement transactions confirm again in X's replacement, at X's height, where their payload does not count
//!   (D-100), and nothing lands the block left behind.
//! - A path C op whose native transaction confirms in X is force-included by a block anchored on X; then a 2-block
//!   reorg takes X and the block above it, and the native transaction confirms again on the new branch: the node
//!   retreats, and a block on the new branch includes the op again, executed once.
//!
//! Afterwards, in both cases:
//!
//! - no block on the node's main chain is anchored off the BTC best chain;
//! - a new node syncs the original node's main chain from genesis, every block extending its main chain, and takes the
//!   blocks below the block left behind that left the main chain with it as side blocks from sync; the block left
//!   behind is rejected as a new block (its anchor is not on the local BTC best chain) and taken as a side block from
//!   sync;
//! - an independent indexer agrees with the original node height by height;
//! - once the data of the blocks after the retreat has been on BTC for `BTC_FINALITY_CONFS` blocks, the original node
//!   and the indexer finalize past the block left behind (before D-104 their finality stopped below it for good).

mod common;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::BlockHash;
use common::*;
use ksana_btc::client::RpcClient;
use ksana_integration_tests::Btc;
use ksana_mempool::Policy;
use ksana_node::chain::{Accepted, Chain};
use ksana_node::node::{Node, NodeConfig};
use ksana_node::NodeError;
use ksana_primitives::{Block, BtcAnchor, FeeMode, DEVNET};
use ksana_rpc::BlockRef;
use serde_json::json;

/// The block at height `height` on node `fast`'s main chain (read from the block store in-process).
fn stored_block(fast: &Fast, height: u64) -> Block {
    let store = fast.node().chain().store();
    let hash = store.hash_at(height).unwrap().unwrap();
    store.block(&hash).unwrap().unwrap()
}

/// Mines one BTC block and waits for the node to see it; returns it.
fn mine_and_sync(btc: &Btc, fast: &Fast) -> BtcAnchor {
    mine(btc, 1);
    let tip = btc_tip(btc);
    wait("the node to sync BTC", Duration::from_secs(30), || {
        (fast.node().chain().btc().snapshot().unwrap().tip() == Some((tip.height, tip.hash)))
            .then_some(())
    });
    tip
}

/// Reorgs BTC block `from` and every block above it out, and mines `blocks` on the new branch.
fn reorg(btc: &Btc, from: &BtcAnchor, blocks: u64) {
    let hex = BlockHash::from_byte_array(from.hash).to_string();
    btc.call("invalidateblock", &[json!(hex)]);
    mine(btc, blocks);
}

/// Whether `anchor` is on the BTC best chain.
fn on_best_chain(btc: &Btc, anchor: &BtcAnchor) -> bool {
    let hash = btc.call("getblockhash", &[json!(anchor.height)]);
    BlockHash::from_str(hash.as_str().unwrap())
        .unwrap()
        .to_byte_array()
        == anchor.hash
}

/// Waits for the op to be included in a block other than `left`, the block the reorg left behind.
fn wait_included_again(fast: &Fast, op_id: &[u8; 32], left: &[u8; 32]) -> BlockRef {
    wait(
        "the op to be included again",
        Duration::from_secs(60),
        || {
            let block = op_status(fast, op_id)?.block?;
            (block.hash != *left).then_some(block)
        },
    )
}

/// The blocks below `left` that the retreat took off `fast`'s main chain with it, lowest first (each has one parent:
/// the node is the only producer).
fn left_with(fast: &Fast, left: &Block) -> Vec<Block> {
    let store = fast.node().chain().store();
    let mut blocks = Vec::new();
    let mut hash = left.header.parents[0];
    loop {
        let block = store.block(&hash).unwrap().unwrap();
        if store.hash_at(block.header.height).unwrap() == Some(hash) {
            break;
        }
        hash = block.header.parents[0];
        blocks.push(block);
    }
    blocks.reverse();
    blocks
}

/// Takes `block` as a historical block (`accept_block(…, None)`, the same handling as a P2P sync response). This
/// node's BTC sync may lag slightly (an anchor above the local tip is a local fault): retries until it catches up.
fn accept_history(chain: &Chain, block: &Block) -> Accepted {
    wait(
        "syncing the historical block",
        Duration::from_secs(30),
        || match chain.accept_block(block.clone(), None) {
            Ok(report) => Some(report.outcome),
            Err(NodeError::Btc(_)) => None,
            Err(e) => panic!("height {}: {e}", block.header.height),
        },
    )
}

/// The checks after the reorg (see the module docs): `left` is the block the reorg left behind, `last` a block after
/// the retreat whose data is on BTC.
fn check_after_reorg(btc: &Btc, a: &Fast, left: &Block, last: u64) {
    let top = a.tip_height();
    for height in 1..=top {
        let anchor = stored_block(a, height).header.btc_anchor;
        assert!(on_best_chain(btc, &anchor), "height {height}");
    }

    // A new node (no block production, no networking) syncs A's main chain from genesis
    let (url, rpc_user, rpc_pass) = btc_env();
    let dir = tempfile::tempdir().unwrap();
    let b = Node::start(NodeConfig {
        data_dir: dir.path().join("data"),
        genesis: a.genesis.clone(),
        params: DEVNET,
        rpc_addr: "127.0.0.1:0".parse().unwrap(),
        rpc_cors: Default::default(),
        producer: None,
        carrier_payee: None,
        funding: None,
        policy: Policy::default(),
        btc: Arc::new(RpcClient::new(&url, &rpc_user, &rpc_pass).unwrap()),
        btc_poll: Duration::from_millis(100),
        min_block_interval: Duration::from_millis(700),
        net: None,
        pow_algorithm: None,
        pruning: ksana_node::chain::Pruning::default(),
    })
    .unwrap();
    let chain = b.chain();
    for height in 1..=top {
        let block = stored_block(a, height);
        // a block already final as the node started was rebuilt from the data on BTC (D-093)
        let outcome = accept_history(chain, &block);
        assert!(
            matches!(outcome, Accepted::Extended(_) | Accepted::Duplicate),
            "height {height}: {outcome:?}"
        );
        assert_eq!(
            chain.store().hash_at(height).unwrap(),
            Some(block.header.hash()),
            "height {height}"
        );
    }
    // The blocks below the block left behind that left A's main chain with it (in the path B case at least its parent,
    // anchored on X too): side blocks from sync, as a peer gets them from GetBlocks or GetBlock
    for block in left_with(a, left) {
        let height = block.header.height;
        assert_eq!(
            accept_history(chain, &block),
            Accepted::Side,
            "height {height}"
        );
    }
    // The block left behind: rejected as a new block (depends on the local view, not marked invalid), a side block
    // from sync
    match chain.accept_block(left.clone(), Some(ksana_node::unix_now())) {
        Err(NodeError::Rejected(m)) => assert!(m.contains("best chain"), "{m}"),
        other => panic!("the block left behind should be rejected as a new block: {other:?}"),
    }
    assert!(!chain.is_invalid(&left.header.hash()).unwrap());
    assert_eq!(accept_history(chain, left), Accepted::Side);
    assert_eq!(
        chain.tip().unwrap().hash,
        stored_block(a, top).header.hash()
    );

    // The independent indexer agrees with the original node height by height (the node's settled prefix first passes
    // `last` by SETTLEMENT_DEPTH blocks: the indexer executes a main-chain block only once it has that many
    // confirmations, D-063)
    settle_for_indexer(btc, a, last);
    let (ix, _ix_dir) = indexer(a);
    let status = index_and_compare(&ix, last, |h| a.block(h));
    assert!(status.height >= last);

    // Finality goes past the block left behind once the data after the retreat has BTC_FINALITY_CONFS confirmations
    let enough = btc_tip(btc).height + DEVNET.btc_finality_confs;
    while btc_tip(btc).height < enough {
        mine(btc, 1);
    }
    wait(
        "the node to finalize past the retreat",
        Duration::from_secs(60),
        || (a.node().chain().finalized().ok()?.1 >= last).then_some(()),
    );
    wait(
        "the indexer to finalize past the retreat",
        Duration::from_secs(60),
        || (index_once(&ix).finalized_height >= last).then_some(()),
    );
    assert!(last >= left.header.height);
}

#[test]
fn blocks_anchored_to_a_reorged_out_btc_block_leave_the_main_chain() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m61");
    let (alice, bob) = (user("m61/alice"), user("m61/bob"));
    let a = Fast::start(&btc, "m61/reorg", &[(&alice, 1_000)], 6);

    // The op is submitted once the node has produced a block anchored on BTC tip X: blocks are produced one after
    // another and BTC does not move until the reorg, so the op's block anchors X, and so does its parent (an op
    // submitted as soon as the node has seen X can still be taken by an attempt that read the BTC snapshot before X).
    // Its settlement transactions are then broadcast
    let x = mine_and_sync(&btc, &a);
    wait("a block anchored on X", Duration::from_secs(30), || {
        (a.node().chain().tip().unwrap().header.btc_anchor == x).then_some(())
    });
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let op_id = a.submit(&op).unwrap();
    let first = a.wait_included(&op_id).block.unwrap();
    let left = stored_block(&a, first.height);
    assert_eq!(left.header.btc_anchor, x);
    a.wait_settlement(first.height, ksana_rpc::SettlementState::Broadcast);

    // BTC reorgs X out; the old settlement transactions confirm again in X's replacement, where their payload does not
    // count (D-100). The node retreats below the blocks anchored on X and includes the op again on the new branch
    reorg(&btc, &x, 2);
    let again = wait_included_again(&a, &op_id, &first.hash);
    assert!(on_best_chain(
        &btc,
        &stored_block(&a, again.height).header.btc_anchor
    ));

    // A second op, and the data of its block lands
    let op = signed(&alice, 1, FeeMode::Token { amount: 3 }, bob.account, 6);
    let second = a.wait_included(&a.submit(&op).unwrap()).block.unwrap();
    mine_until_prefix(&btc, &a, second.height);
    assert_eq!(a.ksa(&bob), 11);
    check_after_reorg(&btc, &a, &left, second.height);
}

#[test]
fn a_two_block_reorg_moves_the_native_transaction_of_a_path_c_op() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m61c");
    let (alice, bob, carol) = (user("m61c/alice"), user("m61c/bob"), user("m61c/carol"));
    let a = Fast::start(&btc, "m61/native", &[(&alice, 1_000), (&carol, 1_000)], 6);

    // carol's native transaction confirms in X, and a block anchored on X force-includes its op; one more BTC block
    // above X
    let (native, op) = pay_native(&btc, &|input| {
        signed_native(&carol, 0, input, bob.account, 3)
    });
    let op_id = op.op_id().unwrap();
    let x = mine_and_sync(&btc, &a);
    let confirmed_in = btc.call(
        "getrawtransaction",
        &[json!(native.compute_txid().to_string()), json!(true)],
    );
    assert_eq!(
        confirmed_in["blockhash"].as_str(),
        Some(BlockHash::from_byte_array(x.hash).to_string().as_str())
    );
    let first = a.wait_included(&op_id).block.unwrap();
    let left = stored_block(&a, first.height);
    assert_eq!(left.header.btc_anchor, x);
    mine_and_sync(&btc, &a);

    // The reorg takes X and the block above it; the native transaction confirms again in the new branch's first block.
    // The node retreats below the block and force-includes the op again
    reorg(&btc, &x, 3);
    let again = wait_included_again(&a, &op_id, &first.hash);
    assert!(on_best_chain(
        &btc,
        &stored_block(&a, again.height).header.btc_anchor
    ));
    assert_eq!(a.ksa(&bob), 3);
    assert_eq!(a.ksa(&carol), 997);

    // A path B op, and the data of its block lands
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let second = a.wait_included(&a.submit(&op).unwrap()).block.unwrap();
    mine_until_prefix(&btc, &a, second.height);
    assert_eq!(a.ksa(&bob), 8);
    check_after_reorg(&btc, &a, &left, second.height);
}
