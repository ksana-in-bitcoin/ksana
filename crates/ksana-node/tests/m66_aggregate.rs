//! M66: the second halves of a block's path B signatures (D-154), beside the block and outside what is agreed on.
//!
//! - A block is the same block with them, without them and with wrong ones: its hash, its validity and the state it
//!   gives do not depend on them.
//! - When the block leaves the main chain, its ops return to the entry pool with their whole signatures only on a node
//!   that holds the second halves the producer gave; elsewhere they are left out and their senders submit them again.
//! - A block arriving from a peer with second halves that are not one per path B entry is refused without the block
//!   being judged, and the same block with the producer's second halves is accepted afterwards.

mod common;

use std::sync::Arc;

use common::side::{setup, Side};
use common::{transfer_op, user};
use ksana_node::chain::Accepted;
use ksana_node::net::NetHandler;
use ksana_p2p::{Handler, Keypair, Verdict};
use ksana_primitives::{Block, Entry, Params, DEVNET};

/// The merge set limit of [`ksana_node/tests/m7_dag.rs`]'s scenario: a side of 4 blocks cannot be merged and is
/// abandoned.
fn params() -> Params {
    Params {
        max_mergeset_size: 4,
        ..DEVNET
    }
}

#[test]
fn ops_return_to_the_pool_only_where_the_second_halves_are_held() {
    let (carol, dave) = (user("m66/carol"), user("m66/dave"));
    let (mock, genesis) = setup(&[(&carol, 1_000)]);
    let side = |name: &str| Side::with_params(&mock, &genesis, name, params());
    let (a, b) = (side("m66a"), side("m66b"));
    let ops = [
        transfer_op(&carol, 0, 2, dave.account, 6),
        transfer_op(&carol, 1, 2, dave.account, 7),
    ];
    for op in &ops {
        b.submit(op.clone());
    }
    let b_blocks: Vec<Block> = (0..4).map(|_| b.mine()).collect();
    let a_blocks: Vec<Block> = (0..6).map(|_| a.mine()).collect();
    let first = &b_blocks[0];
    assert!(matches!(
        first.entries.as_slice(),
        [
            Entry::Sponsored { .. },
            Entry::Sponsored { .. },
            Entry::Aggregate { .. }
        ]
    ));
    assert_eq!(first.second_halves.len(), 2);

    // The same block as three nodes come to hold it: as its producer gave it, without the second halves (as a block
    // rebuilt from BTC is), and with second halves that belong to no signature
    let bare = Block {
        second_halves: Vec::new(),
        ..first.clone()
    };
    let mut wrong = first.clone();
    wrong.second_halves.swap(0, 1);
    for (name, copy, returned) in [
        ("m66-held", first, true),
        ("m66-bare", &bare, false),
        ("m66-wrong", &wrong, false),
    ] {
        let node = side(name);
        assert_eq!(copy.header.hash(), first.header.hash());
        assert!(matches!(
            node.accept(copy).unwrap().outcome,
            Accepted::Extended(_)
        ));
        for block in &b_blocks[1..] {
            node.accept(block).unwrap();
        }
        assert_eq!(node.chain.tip().unwrap(), b.chain.tip().unwrap(), "{name}");
        assert_eq!(node.ksa(&dave), 13, "{name}");

        // The node switches to A's chain: the block leaves the main chain
        for block in &a_blocks {
            node.accept(block).unwrap();
        }
        assert_eq!(node.chain.tip().unwrap(), a.chain.tip().unwrap(), "{name}");
        assert_eq!(node.ksa(&dave), 0, "{name}");
        for op in &ops {
            assert_eq!(node.pending(op), returned, "{name}");
        }
        assert_eq!(
            node.mempool.lock().unwrap().len(),
            if returned { 2 } else { 0 },
            "{name}"
        );
        // With the ops back, the next block includes them again; without them it is empty
        let next = node.mine();
        assert_eq!(next.entries.len(), if returned { 3 } else { 0 }, "{name}");
        assert_eq!(node.ksa(&dave), if returned { 13 } else { 0 }, "{name}");
    }
}

#[test]
fn second_halves_of_the_wrong_number_are_refused_without_judging_the_block() {
    let (carol, dave) = (user("m66/carol2"), user("m66/dave2"));
    let (mock, genesis) = setup(&[(&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, "m66a2");
    let v = Side::new(&mock, &genesis, "m66v2");
    a.submit(transfer_op(&carol, 0, 2, dave.account, 6));
    let block = a.mine();
    v.sync();
    let handler = NetHandler {
        chain: Arc::clone(&v.chain),
        mempool: Arc::clone(&v.mempool),
        events: v.producer.events.clone(),
    };
    let peer = Keypair::generate_ed25519().public().to_peer_id();
    let hash = block.header.hash();

    for count in [2, 3] {
        let padded = Block {
            second_halves: vec![[1; 32]; count],
            ..block.clone()
        };
        let verdict = handler.on_block(peer, padded, false);
        assert!(
            matches!(&verdict, Verdict::Reject(m) if m.contains("second halves")),
            "{verdict:?}"
        );
        assert!(!v.chain.is_invalid(&hash).unwrap());
        assert!(!v.chain.contains(&hash).unwrap());
    }
    assert_eq!(handler.on_block(peer, block, false), Verdict::Accept);
    assert_eq!(v.chain.tip().unwrap().hash, hash);
    assert_eq!(v.ksa(&dave), 6);
}
