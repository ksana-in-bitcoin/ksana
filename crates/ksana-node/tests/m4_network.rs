//! M4 / M7 acceptance (3 in-process nodes connected locally over a real libp2p network; BTC is the in-memory mock chain
//! `MockBtc` shared by the three nodes):
//!
//! 1. 3 nodes produce blocks concurrently at high frequency: parallel blocks are merged by later blocks, no orphan
//!    blocks arise, and the state converges (D-063);
//! 2. a fork is created by partitioning the network, then reconnecting: nodes switch to the chain with more blue work,
//!    later blocks merge the other side, and the ops of both sides execute (of conflicting ones only one executes).
//!
//! The former item 3 (no mutually conflicting settlement transactions are broadcast to BTC during the fork) targeted
//! path A: the settlement transactions of both sides would spend the same user funding input. Path A has been deleted
//! (D-078), settlement transactions spend only each producer's own funding, and this item no longer applies.
//!
//! The same scenario on real bitcoind is in `tests/integration/tests/m4_regtest.rs`.

mod common;

use std::time::Duration;

use common::net::{producers, wait, Net, TestNode};
use common::{transfer_op, user};
use ksana_rpc::{KsanaApiClient, OpState};

/// No orphan blocks arise (D-063): 3 nodes produce blocks concurrently at high frequency and parallel blocks appear
/// frequently; every produced block is merged by a later block and enters the past of the main-chain tip, and the DAG,
/// main chain and state of all nodes agree.
#[test]
fn three_nodes_mine_concurrently_and_converge() {
    let (alice, bob) = (user("m4/net/alice"), user("m4/net/bob"));
    let mut net = Net::start(3, &[(&alice, 1_000_000)], Duration::from_millis(100));
    for node in &net.nodes {
        node.pause(false);
    }
    // The ops are submitted to different nodes and reach every node's entry pool via gossip
    let ops: Vec<[u8; 32]> = net
        .nodes
        .iter()
        .zip(0u64..)
        .map(|(node, nonce)| node.submit(&transfer_op(&alice, nonce, 2, bob.account, 10)))
        .collect();
    let start = net.nodes.iter().map(TestNode::height).max().unwrap();
    // The three nodes produce blocks simultaneously until every node's blocks have entered the main chain (not
    // counting the last 3 blocks) and parallel blocks have appeared
    wait(
        "three nodes produce blocks simultaneously",
        Duration::from_secs(120),
        || {
            let high = net.nodes.iter().all(|n| n.height() >= start + 20);
            let chain = net.nodes[0].node().chain();
            let parallel = chain
                .main_chain()
                .unwrap()
                .entries()
                .iter()
                .any(|e| !e.merged.is_empty());
            (high && parallel && producers(&net.nodes[0], 3).len() == 3).then_some(())
        },
    );
    // Stop B and C; A produces more blocks until they have merged all parallel blocks (each node's DAG has one tip)
    net.nodes[1].pause(true);
    net.nodes[2].pause(true);
    let mut tip = None;
    for _ in 0..5 {
        net.mine_on(0, 1);
        let t = net.converge();
        if net
            .nodes
            .iter()
            .all(|n| n.node().chain().dag_tips().unwrap() == vec![t.hash])
        {
            tip = Some(t);
            break;
        }
    }
    let tip = tip.expect("all parallel blocks merged");

    let dag_len = net.nodes[0].node().chain().dag_len().unwrap();
    for node in &net.nodes {
        let chain = node.node().chain();
        assert_eq!(node.tip(), tip);
        assert_eq!(chain.dag_len().unwrap(), dag_len);
        // Every block is accepted by the main chain: parallel blocks are merged instead of becoming orphans
        for hash in chain.store().dag_order().unwrap() {
            assert!(chain.accepted_height(&hash).unwrap().is_some());
        }
        assert_eq!(node.ksa(&bob), 30);
        assert_eq!(node.nonce(&alice), 3);
        for op_id in &ops {
            assert_eq!(node.status(op_id).unwrap().status, OpState::FastConfirmed);
        }
    }
    // Blocks produced by all three nodes are on the final main chain: they produced simultaneously and accepted each
    // other's blocks
    for node in &net.nodes {
        assert_eq!(producers(node, 0).len(), 3);
    }

    // A new node syncs from the genesis block (/ksana/sync/2, D-063): it gets the same DAG, main chain and state
    let late = net.join();
    wait(
        "new node syncs to the same tip",
        Duration::from_secs(60),
        || (net.nodes[late].tip() == tip).then_some(()),
    );
    let (old, new) = (net.nodes[0].node().chain(), net.nodes[late].node().chain());
    assert_eq!(new.dag_len().unwrap(), dag_len);
    assert_eq!(new.dag_tips().unwrap(), vec![tip.hash]);
    assert_eq!(
        new.main_chain().unwrap().entries(),
        old.main_chain().unwrap().entries()
    );
    for hash in old.store().dag_order().unwrap() {
        assert_eq!(
            new.accepted_height(&hash).unwrap(),
            old.accepted_height(&hash).unwrap()
        );
    }
    let new_node = &net.nodes[late];
    assert_eq!(new_node.ksa(&bob), 30);
    assert_eq!(new_node.nonce(&alice), 3);
}

/// Reconnect after a partition (D-063): A syncs to B's chain, which has more blue work (the state rolls back to the
/// fork point and re-executes along B's chain), then A's later block merges A's own former branch: the ops of both
/// sides execute, of the conflicting ops with the same nonce only the one on the main chain executes and the other is
/// skipped.
#[test]
fn partition_then_reconnect_merges_both_sides() {
    let (alice, bob, carol, dave, erin) = (
        user("m4/net/alice2"),
        user("m4/net/bob2"),
        user("m4/net/carol2"),
        user("m4/net/dave2"),
        user("m4/net/erin2"),
    );
    let net = Net::start(
        3,
        &[(&alice, 1_000), (&dave, 1_000)],
        Duration::from_millis(200),
    );
    net.mine_on(0, 2);
    let fork = net.converge();

    // Partition A from B and C; the same nonce is spent once on each side, and A's side has another non-conflicting op
    net.partition(&[0], &[1, 2]);
    let to_bob = net.nodes[0].submit(&transfer_op(&alice, 0, 3, bob.account, 10));
    let to_erin = net.nodes[0].submit(&transfer_op(&dave, 0, 2, erin.account, 5));
    let to_carol = net.nodes[1].submit(&transfer_op(&alice, 0, 4, carol.account, 20));
    net.mine_on(0, 2);
    net.mine_on(1, 4);
    let (a, b, c) = (&net.nodes[0], &net.nodes[1], &net.nodes[2]);
    wait("C catches up with B", Duration::from_secs(30), || {
        (c.tip().hash == b.tip().hash).then_some(())
    });
    assert_eq!(a.height(), fork.header.height + 2);
    assert_eq!(b.height(), fork.header.height + 4);
    assert_eq!(a.ksa(&bob), 10);
    assert_eq!(a.ksa(&erin), 5);
    assert_eq!(b.ksa(&carol), 20);
    let a_blocks: Vec<[u8; 32]> = (fork.header.height + 1..=a.height())
        .map(|h| a.node().chain().store().hash_at(h).unwrap().unwrap())
        .collect();

    // Reconnect: A syncs to B's chain with more blue work; the state rolls back to the fork point and re-executes along
    // B's chain
    net.heal(&[0], &[1, 2]);
    let tip = net.converge();
    assert_eq!(tip, b.tip());
    assert_eq!(a.ksa(&bob), 0);
    assert_eq!(a.ksa(&erin), 0);
    assert_eq!(a.ksa(&carol), 20);
    for (h, hash) in (fork.header.height + 1..).zip(&a_blocks) {
        assert_ne!(
            a.node().chain().store().hash_at(h).unwrap().as_ref(),
            Some(hash),
            "A's own blocks are no longer on the main chain"
        );
    }

    // A resumes producing: the new block has B's tip as selected parent and merges A's former branch at the same time
    net.mine_on(0, 1);
    let next = net.converge();
    assert_eq!(next.header.parents, vec![tip.hash, a_blocks[1]]);
    assert_eq!(next.header.producer, user("m4/net/producer/0").account);
    for node in &net.nodes {
        let chain = node.node().chain();
        for hash in &a_blocks {
            assert_eq!(
                chain.accepted_height(hash).unwrap(),
                Some(next.header.height),
                "A's former blocks are merged by the new block"
            );
        }
        // Of the conflicting transfers only the one on the main chain executes; the non-conflicting one executes as
        // usual
        assert_eq!(node.ksa(&carol), 20);
        assert_eq!(node.ksa(&bob), 0);
        assert_eq!(node.ksa(&erin), 5);
        assert_eq!(node.ksa(&alice), 1_000 - 20 - 4);
        assert_eq!(node.ksa(&dave), 1_000 - 5 - 2);
        assert_eq!(node.nonce(&alice), 1);
        assert_eq!(node.nonce(&dave), 1);
        assert_eq!(
            node.status(&to_carol).unwrap().status,
            OpState::FastConfirmed
        );
        assert_eq!(
            node.status(&to_erin).unwrap().status,
            OpState::FastConfirmed
        );
        let skipped = chain.store().op_location(&to_bob).unwrap().unwrap();
        assert!(
            skipped.skipped,
            "the merged conflicting transfer is skipped"
        );
        // RPC: the skipped op, the merged block and the block that merges it (D-063)
        let status = node.status(&to_bob).unwrap();
        assert_eq!(status.status, OpState::Skipped);
        assert_eq!(status.block.unwrap().height, next.header.height);
        let merged = node
            .rt
            .block_on(node.client.get_block(hex::encode(a_blocks[0])))
            .unwrap();
        assert_eq!(merged.accepted_height, Some(next.header.height));
        assert!(merged.receipts[0]
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("skipped")));
        let merging = node
            .rt
            .block_on(node.client.get_block(next.header.height.to_string()))
            .unwrap();
        assert_eq!(merging.hash, next.hash);
        assert_eq!(merging.merged, a_blocks);
        assert_eq!(merging.accepted_height, Some(next.header.height));
    }
}
