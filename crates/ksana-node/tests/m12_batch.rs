//! M12: settlement batching (D-077). In-process node + in-memory mock BTC chain.
//!
//! - When no settlement transaction is in flight, a block holding path-B ops carries its own settlement transaction;
//!   while it is in flight, later blocks carry none and their bodies back up; the next block after it confirms carries
//!   one batch settlement transaction carrying the backlogged headers and bodies, and once confirmed these blocks enter
//!   the settled closure. Prints a comparison of BTC transaction count and bytes against per-block settlement.
//! - When the bodies backlogged during the flight exceed the threshold, it is no longer waited for: the next block
//!   carries a settlement transaction as usual, and none of the backlogged bodies is squeezed out.
//! - With batching off (`--no-settlement-batching`), every block that needs a body carries its own settlement
//!   transaction.
//! - Block validity: a body carried by a settlement payload must match that block's entries.
//! - Republish: when a side block parallel to empty blocks and carrying a settlement transaction lands on BTC first,
//!   the empty blocks' headers are not republished (they are not in that side block's past).

mod common;

use std::sync::Arc;
use std::time::Duration;

use bitcoin::hashes::Hash as _;
use common::mock_node::{signed, wait, MockNode};
use common::side::{setup, Side};
use common::{transfer_op, user};
use ksana_btc::client::BtcClient;
use ksana_btc::mock::MockBtc;
use ksana_btc::payload::{attached_payload, bodies_in, body_len, needs_body};
use ksana_btc::settlement::{draft, finalize, ProducerUtxo, SettlementPair};
use ksana_node::chain::PAYLOAD_BODY_TRIGGER;
use ksana_node::rescue::Rescuer;
use ksana_node::NodeError;
use ksana_primitives::{Block, BlockHeader, EntryData, FeeMode, Payload, SettlementTxs, DEVNET};
use ksana_rpc::SettlementState;

/// Whether the main-chain block at this height carries a settlement transaction.
fn carries(n: &MockNode, height: u64) -> bool {
    let store = n.node().chain().store();
    let hash = store.hash_at(height).unwrap().unwrap();
    store.block(&hash).unwrap().unwrap().settlement.is_some()
}

/// The settlement transactions (commit and reveal, D-081) and their payload.
fn decode(txs: &SettlementTxs) -> (SettlementPair, Payload) {
    let pair = SettlementPair::decode(txs).unwrap();
    let payload = attached_payload(&DEVNET, &pair).unwrap();
    let payload = Payload::decode(&payload, &DEVNET).unwrap();
    (pair, payload)
}

/// The total virtual size of the two transactions.
fn vsize(pair: &SettlementPair) -> usize {
    pair.commit.vsize() + pair.reveal.vsize()
}

/// Which blocks' bodies the payload carries.
fn carried(payload: &Payload) -> Vec<[u8; 32]> {
    bodies_in(payload).iter().map(|(hash, _)| *hash).collect()
}

#[test]
fn blocks_during_an_in_flight_settlement_ride_on_one_batch() {
    let (alice, bob) = (user("m12/alice"), user("m12/bob"));
    let n = MockNode::start_with(&[(&alice, 1_000)], Duration::from_millis(50));
    let include = |nonce: u64| -> u64 {
        let op = signed(&alice, nonce, FeeMode::Token { amount: 2 }, bob.account, 1);
        n.wait_included(&n.submit(&op)).block.unwrap().height
    };
    // The first: no settlement transaction in flight, so this block carries its own
    let first = include(0);
    assert!(carries(&n, first));
    let first_settlement = n.wait_settlement(first, SettlementState::Broadcast);
    // Three more while it is in flight: their blocks carry no settlement transaction and the bodies back up
    let deferred: Vec<u64> = (1..=3).map(include).collect();
    for h in &deferred {
        assert!(
            !carries(&n, *h),
            "the block at height {h} should not carry a settlement transaction"
        );
        let s = n.settlement(*h);
        assert_eq!(s.txid, None);
        assert_eq!(s.carrier, None);
        assert_eq!(s.status, SettlementState::Pending);
    }
    // The next block after the first confirms carries the batch settlement transaction, carrying the three blocks'
    // bodies
    n.mock.mine(1);
    let carrier = wait("batch settlement transaction", || {
        n.settlement(deferred[0]).carrier
    });
    assert!(carries(&n, carrier.height));
    let batch = n.settlement(deferred[0]);
    let (batch_tx, payload) = decode(batch.txs.as_ref().unwrap());
    let bodies = carried(&payload);
    for h in &deferred {
        assert_eq!(n.settlement(*h).carrier.map(|c| c.hash), Some(carrier.hash));
        assert!(bodies.contains(&n.block(*h).hash), "height {h}");
    }
    // After the batch settlement confirms, the three blocks' data has landed on BTC and they enter the settled closure
    n.wait_settlement(carrier.height, SettlementState::Broadcast);
    n.mock.mine(1);
    for h in &deferred {
        let s = n.wait_settlement(*h, SettlementState::Confirmed);
        assert_eq!(Some(s.confirmed.unwrap().txid), batch.txid);
    }
    let last = *deferred.last().unwrap();
    wait("enters the settled closure", || {
        n.settlement(last).settled.then_some(())
    });
    assert_eq!(n.ksa(&bob), 4);

    // Cost: 4 blocks with ops used only 2 settlements (two transactions each, commit and reveal). Both approaches land
    // the same data (headers and bodies) on BTC; per-block settlement adds the fixed overhead of each settlement (the
    // inputs and outputs of the commit and reveal transactions, the fixed part of the envelope; the payload in the
    // witness counts 0.25 vB per byte)
    let (first_tx, _) = decode(first_settlement.txs.as_ref().unwrap());
    let overhead = vsize(&first_tx) - attached_payload(&DEVNET, &first_tx).unwrap().len() / 4;
    let batched = vsize(&first_tx) + vsize(&batch_tx);
    let per_block_txs = 1 + deferred.len();
    let per_block = batched + overhead * (per_block_txs - 2);
    eprintln!(
        "settlement batching: 4 blocks with ops used 2 settlements, {batched} vB in total (fixed overhead about {overhead} vB each); \
         per-block settlement needs {per_block_txs}, about {per_block} vB"
    );
    assert!(overhead > 100 && batched < per_block);
}

/// While the first settlement transaction is in flight (no BTC block mined), submit 10 path-B ops at a time: until the
/// backlogged bodies exceed 1 / `PAYLOAD_BODY_TRIGGER` of the payload limit no block carries a settlement transaction,
/// and the first block after they do carries one with all the backlogged bodies (before the trigger the backlog is at
/// most twice the threshold, which fits).
#[test]
fn a_large_backlog_does_not_wait_for_the_settlement_in_flight() {
    let (alice, bob) = (user("m12/large/alice"), user("m12/large/bob"));
    let n = MockNode::start_with(&[(&alice, 100_000)], Duration::from_millis(50));
    let mut nonce = 0u64;
    // Submits `count` ops, waits until they are all included, and returns the highest block they are in
    let mut submit = |count: u64| -> u64 {
        let ids: Vec<[u8; 32]> = (nonce..nonce + count)
            .map(|k| {
                let op = signed(&alice, k, FeeMode::Token { amount: 2 }, bob.account, 1);
                n.submit(&op)
            })
            .collect();
        nonce += count;
        ids.iter()
            .map(|id| n.wait_included(id).block.unwrap().height)
            .max()
            .unwrap()
    };
    let first = submit(1);
    assert!(carries(&n, first));
    n.wait_settlement(first, SettlementState::Broadcast);

    let carrier = (0..30)
        .find_map(|_| {
            let last = submit(10);
            (first + 1..=last).find(|h| carries(&n, *h))
        })
        .expect(
            "a block should carry a settlement transaction once the backlog exceeds the threshold",
        );
    assert_eq!(
        n.settlement(first).status,
        SettlementState::Broadcast,
        "the first settlement transaction is still in flight"
    );

    // The condition for carrying a settlement transaction is exactly that the backlog or this block's body exceeds the
    // threshold
    let threshold = DEVNET.max_block_data_bytes / PAYLOAD_BODY_TRIGGER;
    let own = |h: u64| -> usize {
        let entries = n.block(h).entries;
        if needs_body(&entries) {
            body_len(&entries).unwrap()
        } else {
            0
        }
    };
    let mut pending = 0usize;
    for h in first + 1..carrier {
        assert!(!carries(&n, h), "height {h}");
        assert!(pending <= threshold && own(h) <= threshold, "height {h}");
        pending += own(h);
    }
    assert!(pending > threshold || own(carrier) > threshold);

    // It carries all the backlogged bodies
    let (_, payload) = decode(n.settlement(carrier).txs.as_ref().unwrap());
    let bodies = carried(&payload);
    let deferred: Vec<u64> = (first + 1..carrier).filter(|h| own(*h) > 0).collect();
    for h in &deferred {
        assert!(bodies.contains(&n.block(*h).hash), "body of height {h}");
    }
    eprintln!(
        "with a backlog of {pending} bytes (threshold {threshold}) the block at height {carrier} carries a settlement transaction, carrying the bodies of {} blocks",
        deferred.len()
    );

    // The two settlement transactions confirm in the same BTC block, and these blocks' data has all landed on BTC
    n.wait_settlement(carrier, SettlementState::Broadcast);
    n.mock.mine(1);
    for h in &deferred {
        n.wait_settlement(*h, SettlementState::Confirmed);
    }
}

#[test]
fn without_batching_every_block_with_entries_settles_itself() {
    let (alice, bob) = (user("m12/off/alice"), user("m12/off/bob"));
    let n = MockNode::start_custom(
        &[(&alice, 1_000)],
        Duration::from_millis(50),
        |mock| mock,
        |funding| funding.with_batching(false),
    );
    let mut heights = Vec::new();
    for nonce in 0..3 {
        let op = signed(&alice, nonce, FeeMode::Token { amount: 2 }, bob.account, 1);
        heights.push(n.wait_included(&n.submit(&op)).block.unwrap().height);
        if nonce == 0 {
            // The first one's settlement transaction is in flight
            n.wait_settlement(heights[0], SettlementState::Broadcast);
        }
    }
    for h in heights {
        assert!(carries(&n, h), "height {h}");
        assert_eq!(n.settlement(h).carrier, None);
    }
}

/// Replaces the settlement transaction carried by `block` with one whose payload was modified by `tamper`, re-signed
/// with the same funding input.
fn with_tampered_payload(side: &Side, block: &Block, tamper: impl FnOnce(&mut Payload)) -> Block {
    let funding = side.producer.funding.clone().unwrap();
    let mock = side.chain.btc().client();
    let (pair, mut payload) = decode(block.settlement.as_ref().unwrap());
    tamper(&mut payload);
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

#[test]
fn carried_bodies_must_match_the_blocks() {
    let (alice, bob) = (user("m12/v/alice"), user("m12/v/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::batched(&mock, &genesis, "m12v");
    let b = Side::new(&mock, &genesis, "m12v-b");
    let mut blocks = Vec::new();
    // 1: carries its own settlement transaction; 2: carries none while it is in flight
    a.submit(transfer_op(&alice, 0, 3, bob.account, 1));
    blocks.push(a.mine());
    a.submit(transfer_op(&alice, 1, 3, bob.account, 1));
    blocks.push(a.mine());
    assert!(blocks[0].settlement.is_some() && blocks[1].settlement.is_none());
    for _ in 0..DEVNET.settlement_depth {
        blocks.push(a.mine());
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    // The next block carries the batch settlement transaction, carrying block 2's body
    let batch = a.mine();
    let (_, payload) = decode(batch.settlement.as_ref().unwrap());
    let second = blocks[1].header.hash();
    assert!(carried(&payload).contains(&second));
    for block in &blocks {
        b.accept(block).unwrap();
    }
    // The body was modified: invalid
    let tampered = with_tampered_payload(&a, &batch, |payload| {
        if let Payload::Settlement { bodies, .. } = payload {
            for body in bodies.iter_mut() {
                body.entries.push(EntryData::Unlock {
                    height: 1,
                    index: 0,
                });
            }
        }
    });
    match b.accept(&tampered) {
        Err(NodeError::InvalidBlock(m)) => {
            assert!(m.contains("does not match that block's entries"), "{m}")
        }
        other => panic!("should be judged invalid: {other:?}"),
    }
}

/// The in-flight settlement transaction is stuck in the mempool (e.g. fee rate too low): when it is still unconfirmed
/// after more than 2 BTC blocks, new blocks stop waiting for it and carry a batch settlement transaction as usual.
#[test]
fn a_stuck_settlement_does_not_hold_back_the_next_batch() {
    let (alice, bob) = (user("m12/stuck/alice"), user("m12/stuck/bob"));
    let n = MockNode::start_with(&[(&alice, 1_000)], Duration::from_millis(50));
    let include = |nonce: u64| -> u64 {
        let op = signed(&alice, nonce, FeeMode::Token { amount: 2 }, bob.account, 1);
        n.wait_included(&n.submit(&op)).block.unwrap().height
    };
    let first = include(0);
    let held = n
        .wait_settlement(first, SettlementState::Broadcast)
        .txid
        .unwrap();
    n.mock.hold(&held);
    let deferred = include(1);
    assert!(!carries(&n, deferred));
    n.mock.mine(1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        n.settlement(deferred).carrier,
        None,
        "still waiting for it within one BTC block"
    );
    n.mock.mine(2);
    let carrier = wait("batch settlement transaction", || {
        n.settlement(deferred).carrier
    });
    assert_ne!(carrier.hash, n.block(first).hash);
    n.mock.release(&held);
}

/// The headers of the republish payloads in the mock chain's mempool.
fn republished(mock: &MockBtc) -> Vec<Vec<BlockHeader>> {
    mock.mempool()
        .iter()
        .filter_map(|tx| match ksana_btc::data::tx_payload(&DEVNET, tx) {
            Some((_, Payload::Headers { headers, .. })) => Some(headers),
            _ => None,
        })
        .collect()
}

/// A side block parallel to an empty block and carrying a settlement transaction lands on BTC first: in acceptance
/// order it comes after the empty block, but the empty block is not in its past, so it does not carry it, and this
/// must not trigger a republish of the empty block's header; the empty block's header lands on BTC with a later
/// settlement transaction whose past contains it (D-047, D-063, D-077).
#[test]
fn a_parallel_carrier_landing_first_does_not_trigger_a_republish() {
    let (alice, bob) = (user("m12/par/alice"), user("m12/par/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::batched(&mock, &genesis, "m12par-a");
    let b = Side::new(&mock, &genesis, "m12par-b");
    // C1: A includes one op and carries its own settlement transaction
    a.submit(transfer_op(&alice, 0, 3, bob.account, 1));
    let c1 = a.mine();
    assert!(c1.settlement.is_some());
    b.accept(&c1).unwrap();
    // A produces two empty blocks E1, E2 on top of C1; meanwhile B produces a block S with a settlement transaction on
    // top of C1, parallel to them
    let e1 = a.mine();
    let e2 = a.mine();
    assert!(e1.entries.is_empty() && e2.entries.is_empty());
    b.submit(transfer_op(&alice, 1, 3, bob.account, 1));
    let s = b.mine();
    assert!(s.settlement.is_some());
    assert_eq!(s.header.parents, vec![c1.header.hash()]);
    // A merges S: the main chain is C1, E1, E2, E3, ..., and in acceptance order the first block with a settlement
    // transaction after E1 is S
    a.accept(&s).unwrap();
    let e3 = a.mine();
    assert_eq!(
        a.chain.accepted_height(&s.header.hash()).unwrap(),
        Some(e3.header.height)
    );
    for _ in 0..DEVNET.settlement_depth {
        a.mine();
    }
    // The settlement transactions of C1 and S land on BTC, E1's header has not yet
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(snapshot.settlement_of(&c1.header.hash()).is_some());
    assert!(snapshot.settlement_of(&s.header.hash()).is_some());
    assert!(snapshot.counted_at(&e1.header.hash()).is_none());

    // The rescue thread does not republish
    let rescuer = Rescuer::new(
        Arc::clone(&a.chain),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    rescuer.run_once().unwrap();
    assert!(republished(&mock).is_empty(), "should not republish");

    // A's next block with ops carries a settlement transaction, carrying the headers of E1, E2, etc.; once it lands,
    // their data has all landed on BTC
    a.submit(transfer_op(&alice, 2, 3, bob.account, 1));
    let x = a.mine();
    let (_, payload) = decode(x.settlement.as_ref().unwrap());
    let Payload::Settlement { headers, .. } = &payload else {
        panic!("settlement payload: {payload:?}");
    };
    let carried: Vec<_> = headers.iter().map(BlockHeader::hash).collect();
    assert!(carried.contains(&e1.header.hash()) && carried.contains(&e2.header.hash()));
    for _ in 0..DEVNET.settlement_depth {
        a.mine();
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    assert!(republished(&mock).is_empty(), "should not republish");
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(snapshot.counted_at(&e1.header.hash()).is_some());
    assert!(snapshot.counted_at(&e2.header.hash()).is_some());
}
