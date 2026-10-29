//! M18 cleanup of implementation limitations (D-093): the node rebuilds the blocks it lacks from the data on BTC,
//! without relying on peers to provide them.
//!
//! - For blocks the finality decision waits for (D-082), the header comes from the header DAG in the BTC payloads and
//!   the body is determined by the same rules as the indexer (forced-inclusion entries derived from BTC, the rest taken
//!   from the payload carrying the body, D-077); a rebuilt block carries no settlement transaction and is validated by
//!   all the rules for receiving historical blocks.
//! - A node without peers catches up with the finalized chain from BTC; empty blocks with only the header landed on BTC
//!   (the republish of D-091) can be rebuilt too, and the node switches to the finalized chain accordingly.
//! - Blocks obtained over the network are no longer rebuilt from BTC; after a rebuild, extending and accepting each
//!   other's blocks work as usual.
//! - For the landed state of each block's data that the decision relies on, blocks the node does not have are answered
//!   from the data on BTC (same criterion as the indexer): a block that needs a body while BTC has no payload carrying
//!   it has not landed yet, and the decision does not stall on it — an attacker who sends only the header to BTC and
//!   withholds the body cannot stall the node's finality decision (a gap found by the adversarial test).
//! - A rebuilt block is bound by the anchor height limit of historical blocks (D-101): one anchored above the local BTC
//!   tip − `ANCHOR_DEPTH` waits until BTC advances, and the BTC sync does not fail meanwhile.

mod common;

use common::side::{setup, Side};
use common::{transfer_op, user};
use std::sync::Arc;

use bitcoin::{
    absolute, transaction, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use ksana_btc::mock::MockBtc;
use ksana_btc::payload::entry_data;
use ksana_node::chain::Accepted;
use ksana_node::rescue::Rescuer;
use ksana_primitives::{Block, Body, Params, Payload, DEVNET};

/// Broadcasts the settlement transactions of blocks that have reached `SETTLEMENT_DEPTH` and gives them `confs`
/// confirmations on BTC.
fn settle(side: &Side, mock: &MockBtc, confs: u32) {
    side.sync();
    side.chain.process_settlements().unwrap();
    mock.mine(confs);
    side.sync();
}

/// A transaction with a KSN1 envelope in its witness (D-081): it spends fresh funds on the mock chain, and the witness
/// is a placeholder signature, the envelope leaf and a placeholder control block (the mock chain checks neither scripts
/// nor signatures; the tracker only looks at the witness syntax). In the tests it plays a republish payload that an
/// attacker or a third party sends to BTC.
fn envelope_tx(mock: &MockBtc, payload: &Payload) -> Transaction {
    envelope_tx_spending(envelope_input(mock), payload)
}

/// Funds the input of an envelope transaction ahead of time: on the mock chain the funding is confirmed in a block of
/// its own.
fn envelope_input(mock: &MockBtc) -> OutPoint {
    mock.fund(&ksana_btc::mock::coinbase_script(), &[10_000])[0]
}

/// [`envelope_tx`] spending an input funded beforehand ([`envelope_input`]).
fn envelope_tx_spending(input: OutPoint, payload: &Payload) -> Transaction {
    let key =
        bitcoin::key::Keypair::from_seckey_slice(&bitcoin::key::Secp256k1::new(), &[0x5a; 32])
            .unwrap()
            .x_only_public_key()
            .0;
    let encoded = payload.encode(&DEVNET).unwrap();
    let leaf = ksana_btc::envelope::envelope_script(&key, &DEVNET.magic, &encoded).unwrap();
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: input,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[vec![0u8; 64], leaf.into_bytes(), vec![0xc0; 33]]),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(ksana_btc::settlement::P2A_AMOUNT_SAT),
            script_pubkey: ksana_btc::settlement::p2a_script(),
        }],
    }
}

/// B produces blocks and settles, and A never receives B's blocks over the network: when syncing BTC, A finds that B's
/// block 1 is final while it does not have it, rebuilds it from the settlement payload (body taken from the payload, no
/// settlement transaction) and attaches it; later B's blocks 2-4 (two empty blocks with only headers, block 4 with a
/// transfer) land on BTC with block 4's settlement, and A rebuilds them likewise. After the rebuild A and B accept each
/// other's new blocks as usual.
#[test]
fn a_node_without_peers_rebuilds_finalized_blocks_from_btc() {
    let (alice, carol) = (user("m18/alice4"), user("m18/carol4"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m18a4");
    let b = Side::new(&mock, &genesis, "m18b4");

    // B: block 1 transfers to carol, then two empty blocks so it reaches SETTLEMENT_DEPTH; the settlement lands on BTC
    // and reaches BTC_FINALITY_CONFS confirmations
    b.submit(transfer_op(&alice, 0, 4, carol.account, 25));
    let b_blocks: Vec<Block> = (0..3).map(|_| b.mine()).collect();
    let b1 = b_blocks[0].header.hash();
    assert!(b_blocks[0].settlement.is_some());
    settle(&b, &mock, DEVNET.btc_finality_confs);
    assert_eq!(b.chain.finalized().unwrap(), (b1, 1));

    // A has received no blocks: after syncing BTC it rebuilds block 1 and attaches it, and the finality decision
    // advances accordingly
    assert_eq!(a.chain.tip().unwrap().header.height, 0);
    a.sync();
    assert_eq!(a.chain.tip().unwrap().hash, b1);
    assert_eq!(a.chain.finalized().unwrap(), (b1, 1));
    assert!(a.chain.missing_parents().unwrap().is_empty());
    assert_eq!(a.ksa(&carol), 25);
    assert_eq!(a.nonce(&alice), 1);
    let rebuilt = a.chain.store().block(&b1).unwrap().unwrap();
    assert_eq!(rebuilt.header, b_blocks[0].header);
    assert_eq!(rebuilt.entries, b_blocks[0].entries);
    assert!(
        rebuilt.settlement.is_none(),
        "a rebuilt block carries no settlement transaction (D-077)"
    );
    // The headers of blocks 2 and 3 (empty) have not landed on BTC yet: A does not have them and is not waiting
    // for them
    assert_eq!(a.chain.finalized().unwrap().1, 1);
    assert!(!a.chain.contains(&b_blocks[1].header.hash()).unwrap());

    // B: block 4 transfers to carol, then two empty blocks; the settlement lands on BTC: the payload carries the
    // headers of blocks 2-4 and the body of block 4
    b.submit(transfer_op(&alice, 1, 4, carol.account, 5));
    let more: Vec<Block> = (0..3).map(|_| b.mine()).collect();
    let b4 = more[0].header.hash();
    settle(&b, &mock, DEVNET.btc_finality_confs);
    assert_eq!(b.chain.finalized().unwrap(), (b4, 4));
    a.sync();
    assert_eq!(a.chain.tip().unwrap().hash, b4);
    assert_eq!(a.chain.finalized().unwrap(), (b4, 4));
    assert_eq!(a.ksa(&carol), 30);
    for block in &b_blocks[1..] {
        let rebuilt = a
            .chain
            .store()
            .block(&block.header.hash())
            .unwrap()
            .unwrap();
        assert!(rebuilt.entries.is_empty() && rebuilt.settlement.is_none());
    }

    // Blocks obtained later over the network are accepted as usual (ones already present are duplicates); A produces
    // its own block and B accepts it
    assert!(matches!(
        a.accept(&more[0]).unwrap().outcome,
        Accepted::Duplicate
    ));
    for block in &more[1..] {
        assert!(matches!(
            a.accept(block).unwrap().outcome,
            Accepted::Extended(_)
        ));
    }
    a.submit(transfer_op(&alice, 2, 4, carol.account, 1));
    let a_next = a.mine();
    assert_eq!(a_next.header.parents, vec![more[2].header.hash()]);
    assert!(matches!(
        b.accept(&a_next).unwrap().outcome,
        Accepted::Extended(_)
    ));
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());
    assert_eq!(b.ksa(&carol), 31);
}

/// The D-091 scenario, but B never receives A's blocks over the network: A produces only empty blocks (headers not
/// landed on BTC), B settles a lighter chain with a transfer onto BTC; on seeing the competing chain A immediately
/// republishes its 7 headers, which reach `BTC_FINALITY_CONFS` confirmations first and become final. When syncing BTC,
/// B finds that the final block is A's block 1, which it does not have, rebuilds these 7 empty blocks from the
/// republished headers (empty bodies, no payload needed) and switches to A's chain; B's transfer returns to the entry
/// pool.
#[test]
fn a_node_rebuilds_republished_header_only_blocks_and_switches() {
    let (alice, carol) = (user("m18/alice5"), user("m18/carol5"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m18a5");
    let b = Side::new(&mock, &genesis, "m18b5");
    let rescuer = Rescuer::new(
        Arc::clone(&a.chain),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );

    // A: 7 empty blocks, no settlement transaction, headers do not land on BTC
    let a_blocks: Vec<Block> = (0..7).map(|_| a.mine()).collect();
    let (a1, a_tip) = (a_blocks[0].header.hash(), a_blocks[6].header.hash());
    assert!(a_blocks
        .iter()
        .all(|b| b.entries.is_empty() && b.settlement.is_none()));

    // B: block 1 transfers to carol, then two empty blocks so it reaches SETTLEMENT_DEPTH; the settlement lands on BTC
    let transfer = transfer_op(&alice, 0, 4, carol.account, 20);
    b.submit(transfer.clone());
    let b_blocks: Vec<Block> = (0..3).map(|_| b.mine()).collect();
    let b1 = b_blocks[0].header.hash();
    assert!(b.chain.tip().unwrap().chain_work < a.chain.tip().unwrap().chain_work);
    settle(&b, &mock, 1);
    assert_eq!(b.ksa(&carol), 20);
    // A receives B's blocks (side chain); after one BTC block it still has no carrier of its own and immediately
    // republishes its 7 headers (D-091)
    for block in &b_blocks {
        assert!(matches!(a.accept(block).unwrap().outcome, Accepted::Side));
    }
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(
        snapshot.counted_at(&a_tip).is_some(),
        "the republish has landed on BTC"
    );

    // The republish reaches the confirmation count: A's 7 blocks are final, B's block 1 is not
    mock.mine(DEVNET.btc_finality_confs - 1);
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (a_tip, 7));
    assert!(!a.chain.is_final(&b1, 1).unwrap());

    // B never received A's blocks: when syncing BTC it rebuilds the 7 empty blocks from the republished headers and
    // switches over
    assert!(!b.chain.contains(&a1).unwrap());
    while b.chain.sync_btc().unwrap() {}
    let (connected, disconnected) = b.chain.take_chain_changes().unwrap();
    assert_eq!(
        connected
            .iter()
            .map(|b| b.header.hash())
            .collect::<Vec<_>>(),
        a_blocks.iter().map(|b| b.header.hash()).collect::<Vec<_>>()
    );
    assert_eq!(
        disconnected
            .iter()
            .map(|b| b.header.hash())
            .collect::<Vec<_>>(),
        b_blocks.iter().map(|b| b.header.hash()).collect::<Vec<_>>()
    );
    ksana_node::ops::chain_changed(
        &b.chain,
        &b.mempool,
        &b.producer.events,
        &connected,
        &disconnected,
    )
    .unwrap();
    assert_eq!(b.chain.tip().unwrap().hash, a_tip);
    assert_eq!(b.chain.finalized().unwrap(), (a_tip, 7));
    assert!(b.chain.missing_parents().unwrap().is_empty());
    assert_eq!(b.ksa(&carol), 0);
    assert!(
        b.pending(&transfer),
        "the disconnected transfer returns to the entry pool"
    );
    for block in &a_blocks {
        let rebuilt = b
            .chain
            .store()
            .block(&block.header.hash())
            .unwrap()
            .unwrap();
        assert_eq!(rebuilt.header, block.header);
        assert!(rebuilt.entries.is_empty() && rebuilt.settlement.is_none());
    }

    // Then B produces on A's chain (selected parent is A's tip, merging its own former lighter chain) and A accepts
    let b_next = b.mine();
    assert_eq!(b_next.header.parents[0], a_tip);
    assert!(matches!(
        a.accept(&b_next).unwrap().outcome,
        Accepted::Extended(_)
    ));
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());
    assert_eq!(a.ksa(&carol), 20);
}

/// Attacker M produces in isolation a chain heavier than A's landed part (block 1 spends alice's nonce 0 to carol and
/// needs a body; block 2 is empty, with a large weight), sends only the two headers to BTC and withholds the body (a
/// gap found by the adversarial test: previously, without M's blocks A "could not determine", the decision stalled
/// forever on M's block 1 and kept requesting it from peers, while the indexer moved on as usual). Now A answers from
/// the data on BTC: M's block 1 needs a body and BTC has no payload carrying it, so the data has not landed — the
/// decision neither stalls nor requests; A's own chain becomes final as usual after settling, M's blocks never become
/// final; M's body landing on BTC later does not change the finalized chain either, and A does not need to rebuild it.
#[test]
fn a_header_only_block_whose_body_is_withheld_does_not_stall_finality() {
    let (alice, bob, carol) = (user("m18/alice6"), user("m18/bob6"), user("m18/carol6"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m18a6");
    let m = Side::new(&mock, &genesis, "m18m6");

    // A: block 1 transfers to bob, then two empty blocks so it reaches SETTLEMENT_DEPTH; M: block 1 spends the same
    // nonce to carol, then one empty block (M does not sync BTC afterwards, otherwise it would rebuild A's blocks from
    // A's settlement payload)
    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    let a_blocks: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    let a1 = a_blocks[0].header.hash();
    m.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let m_blocks: Vec<Block> = (0..2).map(|_| m.mine()).collect();
    let (x1, x2) = (m_blocks[0].header.hash(), m_blocks[1].header.hash());
    assert!(m_blocks[0].settlement.is_some(), "block 1 needs a body");
    assert!(m_blocks[1].entries.is_empty());

    // A's block 1 settlement lands on BTC; M's two headers land afterwards (without the body) and reach
    // BTC_FINALITY_CONFS confirmations
    settle(&a, &mock, 1);
    let headers_only = Payload::Headers {
        headers: m_blocks.iter().map(|b| b.header.clone()).collect(),
        bodies: vec![],
        payee: [0; 32],
    };
    mock.submit(envelope_tx(&mock, &headers_only)).unwrap();
    mock.mine(DEVNET.btc_finality_confs);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(
        snapshot.counted_at(&x2).is_some(),
        "M's headers have landed on BTC"
    );
    // Among the landed part M's chain is heavier and is the decision's candidate; its block 1 data has not landed, so
    // nothing becomes final, but the decision keeps advancing to the BTC tip without stalling on M's block 1 or
    // requesting it from peers
    assert_eq!(a.chain.finalized().unwrap().1, 0);
    assert_eq!(
        a.chain.finality_evaluated_height().unwrap(),
        Some(mock.tip_height()),
        "the decision advances to the BTC tip"
    );
    assert!(
        a.chain.missing_parents().unwrap().is_empty(),
        "does not wait for M's block or request it from peers"
    );
    assert!(!a.chain.contains(&x1).unwrap());

    // A keeps producing: block 4 transfers to bob, then two empty blocks; the settlement lands on BTC (the payload
    // carries the headers of blocks 2-4 and the body of block 4), A's chain is heavier, and once confirmed blocks 1-4
    // are final
    a.submit(transfer_op(&alice, 1, 3, bob.account, 5));
    let more: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    let a4 = more[0].header.hash();
    settle(&a, &mock, DEVNET.btc_finality_confs);
    assert_eq!(a.chain.finalized().unwrap(), (a4, 4));
    assert!(a.chain.is_final(&a1, 1).unwrap());
    assert!(!a.chain.is_final(&x1, 1).unwrap());
    assert!(a.chain.missing_parents().unwrap().is_empty());
    assert_eq!(a.ksa(&bob), 15);
    assert_eq!(a.ksa(&carol), 0);

    // Only now does M send block 1's body to BTC: the finalized chain is unchanged; this block is in no candidate's
    // closure, A does not need it and does not rebuild it
    let with_body = Payload::Headers {
        headers: vec![m_blocks[0].header.clone()],
        bodies: vec![Body {
            header: 0,
            entries: entry_data(&m_blocks[0].entries),
        }],
        payee: [0; 32],
    };
    mock.submit(envelope_tx(&mock, &with_body)).unwrap();
    mock.mine(1);
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (a4, 4));
    assert_eq!(
        a.chain.finality_evaluated_height().unwrap(),
        Some(mock.tip_height())
    );
    assert!(!a.chain.contains(&x1).unwrap());
    assert!(a.chain.missing_parents().unwrap().is_empty());
}

/// A rebuilt block is accepted under the rules of historical blocks, which bound its anchor height by the local BTC
/// tip − `ANCHOR_DEPTH` as for a new block (D-101). With `ANCHOR_DEPTH` 2, a block whose header landed in the BTC tip
/// block, anchored just below it, cannot be rebuilt until BTC advances: here an empty block X of a producer anchoring
/// at the tip, the missing parent of an orphan Y. The rebuild is refused for now and tried again at the next BTC sync,
/// and the BTC sync itself goes on: failing it would skip pruning, and a node retrying a block after a BTC sync would
/// drop it.
#[test]
fn a_rebuild_anchored_above_the_tip_minus_anchor_depth_waits_without_failing_the_btc_sync() {
    let params = Params {
        anchor_depth: 2,
        ..DEVNET
    };
    let (mock, genesis) = setup(&[]);
    let b = Side::with_params(&mock, &genesis, "m18b7", params);
    // C anchors at the BTC tip (ANCHOR_DEPTH 0 on devnet)
    let c = Side::new(&mock, &genesis, "m18c7");
    mock.mine(3);
    let input = envelope_input(&mock);
    let x = c.mine();
    let y = c.mine();
    let tip = mock.tip_height();
    assert_eq!(x.header.btc_anchor.height, tip);
    assert!(x.entries.is_empty());
    assert_eq!(y.header.parents, vec![x.header.hash()]);

    // X's header lands in the next BTC block, one above its anchor; Y reaches B as a sync response and waits as an
    // orphan
    let headers_only = Payload::Headers {
        headers: vec![x.header.clone()],
        bodies: vec![],
        payee: [0; 32],
    };
    mock.submit(envelope_tx_spending(input, &headers_only))
        .unwrap();
    mock.mine(1);
    assert_eq!(mock.tip_height(), tip + 1);
    b.sync();
    assert!(matches!(
        b.chain.accept_block(y.clone(), None).unwrap().outcome,
        Accepted::Orphan
    ));
    assert_eq!(b.chain.missing_parents().unwrap(), vec![x.header.hash()]);
    // X's anchor is the BTC tip − 1, above the tip − ANCHOR_DEPTH: its rebuild is refused for now, and the BTC sync
    // does not fail
    b.chain
        .sync_btc()
        .expect("a rebuild refused for now does not fail the BTC sync");
    assert!(!b.chain.contains(&x.header.hash()).unwrap());

    // One more BTC block: X is rebuilt from its header, and Y follows from the orphan pool
    mock.mine(1);
    b.sync();
    assert!(b.chain.contains(&x.header.hash()).unwrap());
    assert_eq!(b.chain.tip().unwrap().hash, y.header.hash());
}
