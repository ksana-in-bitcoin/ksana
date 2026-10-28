//! M25 pruning (D-097), without the network: one producer settles every block on the mock BTC chain, so blocks become
//! BTC-final one after another and the node prunes.
//!
//! - Memory holds only the pruning point and its future: the pruning root is BTC-final, `min_pruning_depth` blue score
//!   above the pruning point, and the whole history still answers from the block store.
//! - A pruned block delivered again is a duplicate; production, settlement and unlocking go on as before.
//! - A restart continues from the pruning checkpoint: the same tip, finalized block, pruning point and block DAG.
//! - A node that never pruned syncs the whole chain from the pruned node (the history below the pruning root served
//!   from its block store) and, re-executing every block, reaches the same tip: pruning changed no result.
//! - A settlement payload may carry only the bodies of blocks in the future of the new block's merge-depth root, so a
//!   pruned node and one that never pruned judge every payload alike.
//! - The RPC's index of the carriers (D-132) answers below the window from the history, as `carrier_of` does.

mod common;

use bitcoin::hashes::Hash as _;
use common::side::{setup, Side};
use common::{transfer_op, user, User};

use ksana_btc::client::BtcClient;
use ksana_btc::mock::MockBtc;
use ksana_btc::payload::{attached_payload, entry_data};
use ksana_btc::settlement::{draft, finalize, ProducerUtxo, SettlementPair};
use ksana_btc::tracker::carrier_of;
use ksana_node::btc::Carriers;
use ksana_node::chain::{Accepted, Chain, ChainOptions, Pruning};
use ksana_node::NodeError;
use ksana_primitives::{Block, Body, Params, Payload, DEVNET};

/// A small DAG: K = 2 and MERGE_DEPTH = 4 make `min_pruning_depth` 3 × (4 + 2) + 1 = 19; data is final after 2 BTC
/// confirmations.
fn small_dag() -> Params {
    let mut params = DEVNET.clone();
    params.ghostdag_k = 2;
    params.merge_depth = 4;
    params.btc_finality_confs = 2;
    params
}

/// Pruning with a margin of 2 BTC blocks, whenever the pruning point can advance by 5 blue score.
fn pruning() -> Pruning {
    Pruning {
        enabled: true,
        btc_margin: 2,
        step: 5,
    }
}

const MIN_PRUNING_DEPTH: u64 = 19;

/// Produces a block carrying one transfer of 1 KSA from `from` (nonce `nonce`) to `to`, broadcasts the settlements
/// that are due and mines one BTC block.
fn step(side: &Side, mock: &MockBtc, from: &User, nonce: u64, to: [u8; 32]) -> Block {
    side.submit(transfer_op(from, nonce, 3, to, 1));
    let block = side.mine();
    side.sync();
    side.chain.process_settlements().unwrap();
    mock.mine(1);
    side.sync();
    block
}

#[test]
fn pruning_bounds_the_dag_and_a_restart_continues_from_the_checkpoint() {
    let (alice, bob) = (user("m25/alice"), user("m25/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let a = Side::pruned(&mock, &genesis, "m25a", small_dag(), pruning());

    let mut blocks: Vec<Block> = (0..45)
        .map(|nonce| step(&a, &mock, &alice, nonce, bob.account))
        .collect();
    let point = a
        .chain
        .pruning_point()
        .unwrap()
        .expect("the node has pruned");
    let (root, root_height) = a.chain.pruning_root().unwrap().unwrap();
    let height_of = |hash: &[u8; 32]| {
        blocks
            .iter()
            .position(|b| b.header.hash() == *hash)
            .map(|i| i as u64 + 1)
            .unwrap()
    };
    let point_height = height_of(&point);
    // A single chain: blue score is height
    assert_eq!(root_height - point_height, MIN_PRUNING_DEPTH);
    assert!(a.chain.is_final(&root, root_height).unwrap());
    let tip = a.chain.tip().unwrap();
    assert_eq!(
        a.chain.dag_len().unwrap() as u64,
        tip.header.height - point_height + 1,
        "the DAG holds the pruning point and its future only"
    );
    let base = a.chain.main_chain().unwrap().base();
    assert!(base > 0 && base <= point_height);
    assert!(a.dir.path().join("pruning.ckpt").exists());

    // The whole history still answers from the block store
    for (i, block) in blocks.iter().enumerate() {
        let (hash, height) = (block.header.hash(), i as u64 + 1);
        assert_eq!(a.chain.main_hash_at(height).unwrap(), Some(hash));
        assert_eq!(a.chain.accepted_height(&hash).unwrap(), Some(height));
        assert!(a.chain.knows(&hash).unwrap());
    }
    // The RPC's index of the carriers (D-132) answers for the blocks below the window from the history too (in a
    // scope of its own: the main chain's history holds the store open)
    {
        let main = a.chain.main_chain().unwrap();
        let snapshot = a.chain.btc().snapshot().unwrap();
        let params = a.chain.params();
        for from in [0, base, main.tip_height()] {
            let mut carriers = Carriers::new(params, &snapshot, &main, from);
            for block in &blocks {
                let hash = block.header.hash();
                assert_eq!(
                    carriers.carrier(&hash).unwrap(),
                    carrier_of(params, &snapshot, &main, None, &hash).unwrap(),
                    "block {hash:?}, index from height {from}"
                );
            }
        }
    }
    // A pruned block delivered again is a duplicate
    assert!(!a.chain.contains(&blocks[0].header.hash()).unwrap());
    assert!(matches!(
        a.accept(&blocks[0]).unwrap().outcome,
        Accepted::Duplicate
    ));
    assert_eq!(a.ksa(&bob), 45);

    // Production, settlement and unlocking go on, and the node prunes again
    blocks.extend((45..55).map(|nonce| step(&a, &mock, &alice, nonce, bob.account)));
    assert_eq!(a.ksa(&bob), 55);
    let (_, later_root) = a.chain.pruning_root().unwrap().unwrap();
    assert!(later_root > root_height, "the pruning root advanced");

    // Restart: continues from the checkpoint
    let tip = a.chain.tip().unwrap();
    let finalized = a.chain.finalized().unwrap();
    let point = a.chain.pruning_point().unwrap();
    let dag_len = a.chain.dag_len().unwrap();
    let Side {
        chain,
        producer,
        dir,
        ..
    } = a;
    drop(producer);
    drop(chain);
    let options = ChainOptions {
        pruning: pruning(),
        ..ChainOptions::default()
    };
    let chain = Chain::open_with(dir.path(), small_dag(), &genesis, mock.clone(), options).unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
    assert_eq!(chain.pruning_point().unwrap(), point);
    assert_eq!(chain.dag_len().unwrap(), dag_len);
    while chain.sync_btc().unwrap() {}
    chain.resume().unwrap();
    assert_eq!(chain.tip().unwrap(), tip);
    assert_eq!(chain.finalized().unwrap(), finalized);

    // A node that never prunes syncs everything from the pruned node and validates every block
    let b = Side::pruned(&mock, &genesis, "m25b", small_dag(), Pruning::disabled());
    loop {
        let hashes = chain
            .sync_hashes(&b.chain.locator().unwrap(), None, 16)
            .unwrap();
        if hashes.is_empty() {
            break;
        }
        for hash in &hashes {
            let block = chain.store().block(hash).unwrap().unwrap();
            b.accept(&block).unwrap();
        }
    }
    assert_eq!(b.chain.tip().unwrap(), tip);
    // Opening B funded its producer in a new BTC block: both decide finality on the same BTC chain
    while chain.sync_btc().unwrap() {}
    b.sync();
    assert_eq!(b.chain.finalized().unwrap(), chain.finalized().unwrap());
    assert_eq!(b.ksa(&bob), 55);
    assert_eq!(b.chain.pruning_point().unwrap(), None);
    assert_eq!(b.chain.dag_len().unwrap() as u64, tip.header.height + 1);

    // A payload carrying the body of a block below the merge-depth root is invalid on a node that never pruned and on
    // the pruned node alike (which no longer has that block in memory)
    b.submit(transfer_op(&alice, 55, 3, bob.account, 1));
    let next = b.mine();
    let tampered = carrying(&b, &next, &blocks[0]);
    let c = Side::pruned(&mock, &genesis, "m25c", small_dag(), Pruning::disabled());
    for block in &blocks {
        c.accept(block).unwrap();
    }
    match c.accept(&tampered) {
        Err(NodeError::InvalidBlock(m)) => assert!(m.contains("merge-depth root"), "{m}"),
        other => panic!("should be invalid: {other:?}"),
    }
    while chain.sync_btc().unwrap() {}
    match chain.accept_block(tampered, None) {
        Err(NodeError::InvalidBlock(m)) => assert!(m.contains("not in this block's past"), "{m}"),
        other => panic!("should be invalid: {other:?}"),
    }
    for node in [&c.chain, &chain] {
        let report = node.accept_block(next.clone(), None).unwrap();
        assert!(matches!(report.outcome, Accepted::Extended(_)));
    }
}

/// Replaces the settlement transactions attached to `block` with ones whose payload also carries the body of `carried`
/// (its header added to the payload's header sequence when missing), re-signed with the same funding input.
fn carrying(side: &Side, block: &Block, carried: &Block) -> Block {
    let funding = side.producer.funding.clone().unwrap();
    let mock = side.chain.btc().client();
    let pair = SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap();
    let payload = attached_payload(&DEVNET, &pair).unwrap();
    let mut payload = Payload::decode(&payload, &DEVNET).unwrap();
    let Payload::Settlement {
        headers, bodies, ..
    } = &mut payload
    else {
        panic!("not a settlement payload");
    };
    let index = match headers.iter().position(|h| *h == carried.header) {
        Some(index) => index,
        None => {
            headers.insert(0, carried.header.clone());
            for body in bodies.iter_mut() {
                body.header += 1;
            }
            0
        }
    };
    let index = u32::try_from(index).unwrap();
    let at = bodies
        .iter()
        .position(|b| b.header > index)
        .unwrap_or(bodies.len());
    bodies.insert(
        at,
        Body {
            header: index,
            entries: entry_data(&carried.entries),
        },
    );
    let payload = payload.encode(&DEVNET).unwrap();
    let outpoint = pair.commit.input[0].previous_output;
    let prev = mock
        .transaction(&outpoint.txid.to_byte_array())
        .unwrap()
        .unwrap();
    let utxo = ProducerUtxo {
        outpoint,
        txout: prev.tx.output[outpoint.vout as usize].clone(),
    };
    let unsigned = draft(
        &DEVNET,
        &[utxo],
        funding.script.clone(),
        &funding.keypair.x_only_public_key().0,
        &payload,
        2,
    )
    .unwrap();
    let signed = finalize(&DEVNET, unsigned, &payload, &funding.keypair).unwrap();
    Block {
        settlement: Some(signed.encode()),
        ..block.clone()
    }
}

/// The merge-depth root of A's next block (selected parent at blue score 8, `MERGE_DEPTH` 4) is block 4: its payload
/// may carry block 7's body but not block 1's, although both are in its past. Neither node has pruned.
#[test]
fn a_carried_body_below_the_merge_depth_root_is_invalid() {
    let (alice, bob) = (user("m25/root/alice"), user("m25/root/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let a = Side::pruned(
        &mock,
        &genesis,
        "m25root-a",
        small_dag(),
        Pruning::disabled(),
    );
    let b = Side::pruned(
        &mock,
        &genesis,
        "m25root-b",
        small_dag(),
        Pruning::disabled(),
    );
    let blocks: Vec<Block> = (0..8)
        .map(|nonce| step(&a, &mock, &alice, nonce, bob.account))
        .collect();
    for block in &blocks {
        b.accept(block).unwrap();
    }
    a.submit(transfer_op(&alice, 8, 3, bob.account, 1));
    let next = a.mine();
    assert!(next.settlement.is_some());

    match b.accept(&carrying(&a, &next, &blocks[0])) {
        Err(NodeError::InvalidBlock(m)) => assert!(m.contains("merge-depth root"), "{m}"),
        other => panic!("should be invalid: {other:?}"),
    }
    // The attachment is not part of the block's identity: the block itself is not marked invalid
    let report = b.accept(&carrying(&a, &next, &blocks[6])).unwrap();
    assert!(matches!(report.outcome, Accepted::Extended(_)));
    assert_eq!(b.chain.tip().unwrap().hash, next.header.hash());
}

/// D-104: a BTC reorg that takes main-chain blocks off the BTC best chain's anchors does not stop pruning. A block
/// anchored on the orphaned BTC block can never be final, so before the retreat the finalized tip, and the pruning root
/// that follows it, stopped below such a block for good while the kept state grew; the node now retreats below it, and
/// the pruning root goes past it once the blocks after the retreat are final.
#[test]
fn pruning_goes_on_after_a_retreat() {
    let (alice, bob) = (user("m25/alice6"), user("m25/bob6"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let a = Side::pruned(&mock, &genesis, "m25a6", small_dag(), pruning());
    for nonce in 0..30 {
        step(&a, &mock, &alice, nonce, bob.account);
    }
    let (_, root_before) = a
        .chain
        .pruning_root()
        .unwrap()
        .expect("the node has pruned");

    // a block anchored on the BTC tip X, and X reorged out: the node retreats below it, and its op comes back
    a.submit(transfer_op(&alice, 30, 3, bob.account, 1));
    let left = a.mine();
    mock.reorg(1);
    mock.mine(2);
    a.sync();
    assert_eq!(
        Some(&a.chain.tip().unwrap().hash),
        left.header.selected_parent()
    );
    assert!(a.pending(&transfer_op(&alice, 30, 3, bob.account, 1)));

    for nonce in 31..61 {
        step(&a, &mock, &alice, nonce, bob.account);
    }
    assert_eq!(a.ksa(&bob), 61);
    let (root, root_height) = a.chain.pruning_root().unwrap().unwrap();
    assert!(
        root_height > root_before.max(left.header.height),
        "the pruning root passed the retreat"
    );
    assert_ne!(
        a.chain.main_hash_at(left.header.height).unwrap(),
        Some(left.header.hash())
    );
    assert!(a.chain.is_final(&root, root_height).unwrap());
}
