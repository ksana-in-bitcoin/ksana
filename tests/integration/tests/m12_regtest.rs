//! M12 regtest acceptance (`tests/integration/run.sh`): settlement batching (D-077).
//!
//! The producer has batching enabled (the default). Several consecutive blocks containing only path-B ops while no BTC
//! block is mined: the first carries a settlement transaction, the later blocks do not each carry one until that
//! transaction lands on BTC (in flight, D-077 item 6); after a new BTC block, the next block carries one batch
//! settlement transaction carrying their bodies. bitcoind accepts and confirms both transactions, these blocks enter
//! the settled prefix and the producer unlocks, and the independent indexer takes the bodies from the batch
//! settlement payload, with the state roots agreeing with the node block by block.

mod common;

use std::time::Duration;

use common::*;
use ksana_btc::data::tx_payload;
use ksana_btc::payload::entry_data;
use ksana_integration_tests::Btc;
use ksana_primitives::{FeeMode, Payload, DEVNET};
use ksana_rpc::SettlementState;
use serde_json::json;

#[test]
fn blocks_during_an_in_flight_settlement_ride_on_one_batch() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m12");
    let (alice, bob) = (user("m12/alice"), user("m12/bob"));
    let fast = Fast::start(&btc, "m12/batch", &[(&alice, 10_000)], 6);
    fast.wait_height(1);

    // No BTC block is mined meanwhile; each op is submitted after the previous one is included, one per block
    let mut heights = Vec::new();
    for nonce in 0..4 {
        let op = signed(&alice, nonce, FeeMode::Token { amount: 3 }, bob.account, 1);
        let id = fast.submit(&op).unwrap();
        heights.push(fast.wait_included(&id).block.unwrap().height);
    }
    assert!(heights.windows(2).all(|w| w[0] < w[1]), "{heights:?}");
    let (first, rest) = heights.split_first().unwrap();
    let last = *rest.last().unwrap();

    // The first block carries a settlement transaction; the later blocks carry none until it lands on BTC, and no
    // block carries their bodies yet
    let s1 = fast.wait_settlement(*first, SettlementState::Broadcast);
    assert!(s1.carrier.is_none());
    let t1 = s1
        .txid
        .expect("the first block with ops carries a settlement transaction");
    for h in rest {
        assert!(
            fast.block(*h).settlement.is_none(),
            "height {h} carries no settlement transaction"
        );
        let s = fast.settlement(*h);
        assert_eq!(s.status, SettlementState::Pending, "height {h}");
        assert!(s.txid.is_none() && s.carrier.is_none(), "height {h}: {s:?}");
    }
    // The sponsored fee is recorded at the block's acceptance position when received; the RPC adds the block reward
    // that will be paid at unlock (D-079)
    fast.wait_height(last + 1);
    let last_hash = fast.block(last).hash;
    assert!(fast.locked(&fast.producer, &last_hash).is_some());

    // A new BTC block confirms the first settlement transaction; a later block carries the batch settlement
    // transaction with the bodies of the remaining blocks
    mine(&btc, 1);
    fast.wait_settlement(*first, SettlementState::Confirmed);
    let batch: Vec<_> = rest
        .iter()
        .map(|h| fast.wait_settlement(*h, SettlementState::Broadcast))
        .collect();
    let carrier = batch[0].carrier.clone().expect("carried by a later block");
    let t2 = batch[0].txid.expect("batch settlement transaction");
    assert!(carrier.height > last, "{carrier:?}");
    assert_ne!(t2, t1);
    for s in &batch {
        assert_eq!(s.carrier.as_ref(), Some(&carrier));
        assert_eq!(s.txid, Some(t2));
    }
    // The reveal transaction is in the mempool; its only ancestor is its own commit transaction (D-081)
    let txid_rpc = batch[0].txid_rpc.clone().unwrap();
    let entry = btc.call("getmempoolentry", &[json!(txid_rpc)]);
    assert_eq!(
        entry["ancestorcount"], 2,
        "batch settlement transaction in the mempool: {entry}"
    );

    // The batch settlement transaction's payload carries the bodies of the remaining blocks by height, equal to the
    // node's block entries
    let pair =
        ksana_btc::settlement::SettlementPair::decode(batch[0].txs.as_ref().unwrap()).unwrap();
    let (_, payload) = tx_payload(&DEVNET, &pair.reveal).expect("KSN1 payload");
    let Payload::Settlement {
        headers, bodies, ..
    } = payload
    else {
        panic!("settlement payload: {payload:?}");
    };
    let carried: Vec<u64> = bodies
        .iter()
        .map(|b| headers[usize::try_from(b.header).unwrap()].height)
        .collect();
    assert_eq!(carried, rest.to_vec());
    for (body, h) in bodies.iter().zip(rest) {
        assert_eq!(
            body.entries,
            entry_data(&fast.block(*h).entries),
            "height {h}"
        );
    }
    let per_block = heights.len();
    println!("{per_block} blocks with ops: 2 settlement transactions ({per_block} with per-block settlement)");

    // The batch settlement transaction confirms: the remaining blocks' data lands on BTC
    mine(&btc, 1);
    for h in rest {
        let s = fast.wait_settlement(*h, SettlementState::Confirmed);
        assert_eq!(s.confirmed.map(|c| c.txid), Some(t2), "height {h}");
    }

    // After entering the settled prefix the producer unlocks as usual; the indexer takes the bodies from the batch
    // payload, and the state_root at every height agrees with the node
    mine_until_prefix(&btc, &fast, last);
    wait("the unlock", Duration::from_secs(60), || {
        fast.locked(&fast.producer, &last_hash)
            .is_none()
            .then_some(())
    });
    assert!(fast.ksa(&fast.producer) > 0);
    settle_for_indexer(&btc, &fast, last);
    let (ix, _dir) = indexer(&fast);
    index_and_compare(&ix, last, |h| fast.block(h));
}
