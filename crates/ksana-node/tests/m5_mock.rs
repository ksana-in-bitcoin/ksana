//! Node behavior of M5 (in-process node + in-memory mock BTC chain `MockBtc`, no bitcoind needed): settlement backlog
//! limit, rescue settlement, header republish (fee arrears were deleted along with path A, D-078).
//! The acceptance tests on real bitcoind are in `tests/integration/tests/m5_regtest.rs`.

mod common;

use common::mock_node::{signed, wait, MockNode};
use common::user;
use ksana_consensus::Sha256d;
use ksana_primitives::{Entry, FeeMode, DEVNET};
use ksana_rpc::{KsanaApiClient, OpState, SettlementState};

/// Settlement timeout (§6.4, D-046): when the settlement transaction fails to land on BTC for a long time, the fast
/// lane does not roll back, later blocks hold only forced-inclusion entries and new ops stay in the entry pool; once
/// the settlement lands on BTC the fast path resumes automatically.
#[test]
fn settlement_timeout_pauses_the_fast_path_until_the_settlement_confirms() {
    let (alice, bob) = (user("m5/alice"), user("m5/bob"));
    let n = MockNode::start(&[(&alice, 1_000)]);
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let stuck = n.wait_included(&n.submit(&op)).block.unwrap();
    let stuck_block = n.block(stuck.height);
    // The settlement transaction enters the mempool and is never packed (simulating a fee rate too low)
    let s = n.wait_settlement(stuck.height, SettlementState::Broadcast);
    n.mock
        .hold(&s.txid.expect("the block carries a settlement transaction"));
    let timeout_at = stuck_block.header.btc_anchor.height + DEVNET.settlement_timeout;
    while n.mock.tip_height() < timeout_at {
        n.mock.mine(1);
    }
    // Blocks anchored at or after the timeout height hold only forced-inclusion entries; new path-B ops stay in the
    // entry pool
    let first_paused = wait("block anchored at the timeout height", || {
        let tip = n.tip_height();
        (n.block(tip).header.btc_anchor.height >= timeout_at).then_some(tip)
    });
    let op2 = signed(&alice, 1, FeeMode::Token { amount: 2 }, bob.account, 6);
    let op2_id = n.submit(&op2);
    wait("3 more blocks during the pause", || {
        (n.tip_height() >= first_paused + 3).then_some(())
    });
    for height in first_paused..=n.tip_height() {
        let block = n.block(height);
        if block.header.btc_anchor.height >= timeout_at {
            assert!(
                block
                    .entries
                    .iter()
                    .all(|e| matches!(e, Entry::Forced { .. })),
                "the block at height {height} holds fast-path entries"
            );
            assert!(block.settlement.is_none());
        }
    }
    let status =
        n.rt.block_on(n.client.get_op_status(hex::encode(op2_id)))
            .unwrap();
    assert_eq!(status.status, OpState::Pending);
    // The fast lane does not roll back
    assert_eq!(n.block(stuck.height).hash, stuck.hash);
    assert_eq!(n.ksa(&bob), 5);

    // Resumes automatically once the settlement lands on BTC
    n.mock
        .release(&s.txid.expect("the block carries a settlement transaction"));
    n.mock.mine(1);
    n.wait_settlement(stuck.height, SettlementState::Confirmed);
    let included = n.wait_included(&op2_id);
    assert_eq!(included.status, OpState::FastConfirmed);
    assert_eq!(n.ksa(&bob), 11);
}

/// When more than `MAX_SETTLEMENT_HEADERS` consecutive blocks carry no settlement transaction, later settlement
/// payloads cannot fit the earlier headers (they would never land on BTC with a settlement), so the node sends a
/// `Headers` transaction to republish them; once the republish lands, the settled prefix moves past this run of empty
/// blocks (§7.1, D-047).
#[test]
fn missing_headers_are_republished_and_the_settled_prefix_advances() {
    use std::time::Duration;

    let (alice, bob) = (user("m5/alice3"), user("m5/bob3"));
    // Over 500 empty blocks: once the streak weight reaches 120 the network algorithm Argon2id takes about 0.25 s per
    // block (D-094), so the test algorithm Sha256d is used to produce blocks
    let n = MockNode::start_with_algorithm(
        &[(&alice, 1_000)],
        Duration::from_millis(10),
        |mock| mock,
        std::sync::Arc::new(Sha256d),
    );
    let gap = DEVNET.max_settlement_headers as u64 + 5;
    wait("enough empty blocks", || {
        (n.tip_height() > gap).then_some(())
    });
    // There are already more than MAX_SETTLEMENT_HEADERS − 1 blocks after the first block: the node republishes the
    // headers starting from the first block
    let headers_tx = wait("header republish transaction", || {
        n.mock.mempool().into_iter().find_map(|tx| {
            match ksana_btc::data::tx_payload(&DEVNET, &tx) {
                Some((_, ksana_primitives::Payload::Headers { headers, .. })) => Some(headers),
                _ => None,
            }
        })
    });
    assert_eq!(headers_tx.first().unwrap().height, 1);
    // Later settlements carry only the most recent headers; once both the republish and the settlement land on BTC,
    // that block enters the settled prefix
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let block = n.wait_included(&n.submit(&op)).block.unwrap();
    n.wait_settlement(block.height, SettlementState::Broadcast);
    n.mock.mine(1);
    n.wait_settlement(block.height, SettlementState::Confirmed);
    wait("prefix advances", || {
        let s = n.settlement(block.height);
        s.settled.then_some(())
    });
}

/// While the settlement of a block with a settlement transaction (here the unlock block) has not landed on BTC, the
/// earlier headers its payload will carry are not republished; after it lands, only the headers of the empty blocks
/// after it beyond `MAX_SETTLEMENT_HEADERS − 1` are republished (D-047).
#[test]
fn headers_carried_by_a_pending_settlement_are_not_republished() {
    use std::time::Duration;

    let headers_txs = |n: &MockNode| -> Vec<Vec<ksana_primitives::BlockHeader>> {
        n.mock
            .mempool()
            .into_iter()
            .filter_map(|tx| match ksana_btc::data::tx_payload(&DEVNET, &tx) {
                Some((_, ksana_primitives::Payload::Headers { headers, .. })) => Some(headers),
                _ => None,
            })
            .collect()
    };
    let (alice, bob) = (user("m5/alice6"), user("m5/bob6"));
    // As above: over 500 empty blocks are needed, so the test algorithm is used to produce blocks
    let n = MockNode::start_with_algorithm(
        &[(&alice, 1_000)],
        Duration::from_millis(10),
        |mock| mock,
        std::sync::Arc::new(Sha256d),
    );
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let paid = n.wait_included(&n.submit(&op)).block.unwrap();
    n.wait_settlement(paid.height, SettlementState::Broadcast);
    n.mock.mine(1);
    n.wait_settlement(paid.height, SettlementState::Confirmed);
    // The producer's token fee is unlocked afterwards: the unlock block carries a settlement transaction; hold it
    let unlock = wait("unlock block", || {
        (paid.height + 1..=n.tip_height()).find(|h| {
            n.block(*h)
                .entries
                .iter()
                .any(|e| matches!(e, Entry::Unlock { .. }))
        })
    });
    let held = n
        .wait_settlement(unlock, SettlementState::Broadcast)
        .txid
        .expect("the block carries a settlement transaction");
    n.mock.hold(&held);
    // There are already more than MAX_SETTLEMENT_HEADERS − 1 blocks after both paid and unlock, but the header of the
    // first block whose data has not landed (after paid) is carried by the unlock block's settlement awaiting landing:
    // no republish
    let keep = DEVNET.max_settlement_headers as u64 - 1;
    wait("enough empty blocks", || {
        (n.tip_height() > unlock + keep + 5).then_some(())
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        headers_txs(&n).is_empty(),
        "headers that a settlement awaiting landing will carry are not republished"
    );

    // After the unlock block's settlement lands on BTC, the empty blocks after it already exceed
    // MAX_SETTLEMENT_HEADERS − 1, and the next block with a settlement transaction (unlocking the block reward) can
    // carry only the most recent MAX_SETTLEMENT_HEADERS − 1: republish from after the unlock block
    n.mock.release(&held);
    n.mock.mine(1);
    n.wait_settlement(unlock, SettlementState::Confirmed);
    let headers = wait("header republish transaction", || headers_txs(&n).pop());
    assert_eq!(headers.first().unwrap().height, unlock + 1);
}

/// 3 nodes; producer A goes offline right after including the op (produces no further blocks, broadcasts nothing).
fn offline_producer_setup(
    seed: &str,
) -> (
    common::net::Net,
    ksana_rpc::BlockRef,
    [u8; 32],
    common::User,
) {
    use common::net::{wait as wait_for, Net};
    use std::time::Duration;

    let (alice, bob) = (user(&format!("{seed}/alice")), user(&format!("{seed}/bob")));
    let net = Net::start(3, &[(&alice, 1_000)], Duration::from_millis(300));
    net.mine_on(0, 1);
    net.converge();
    for node in &net.nodes {
        while node.node().chain().sync_btc().unwrap() {}
    }
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 7);
    let op_id = net.nodes[0].submit(&op);
    net.nodes[0].pause(false);
    let included = wait_for("A includes the op", Duration::from_secs(30), || {
        net.nodes[0]
            .status(&op_id)
            .filter(|s| s.status == OpState::FastConfirmed)
            .and_then(|s| s.block)
    });
    net.nodes[0].pause(true);
    wait_for(
        "the other nodes receive the block",
        Duration::from_secs(30),
        || {
            net.nodes[1..]
                .iter()
                .all(|n| {
                    n.node().chain().store().hash_at(included.height).unwrap()
                        == Some(included.hash)
                })
                .then_some(())
        },
    );
    assert!(
        net.nodes[0].height() < included.height + DEVNET.settlement_depth,
        "the block's depth is insufficient before A goes offline, so A did not broadcast"
    );
    let attached = net.nodes[0]
        .settlement(included.height)
        .txid
        .expect("the block carries a settlement transaction");
    (net, included, attached, bob)
}

/// Spends the first funding input of the block's settlement transaction (e.g. the producer's wallet used this output
/// elsewhere): the conflicting transaction lands on BTC and the original settlement transaction can never be broadcast
/// again (§7.3, D-045). Returns the spent output.
fn spend_settlement_input(net: &common::net::Net, height: u64) -> bitcoin::OutPoint {
    use bitcoin::{absolute, transaction, Amount, Sequence, Transaction, TxIn, TxOut, Witness};

    let attached = ksana_btc::settlement::SettlementPair::decode(
        net.nodes[1]
            .settlement(height)
            .txs
            .as_ref()
            .expect("the block carries a settlement transaction"),
    )
    .unwrap();
    let point = attached.commit.input[0].previous_output;
    net.mock
        .mine_block(vec![Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: point,
                script_sig: bitcoin::ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ksana_btc::mock::coinbase_script(),
            }],
        }])
        .unwrap();
    point
}

/// The producer is offline (§7.2, D-037): once the block reaches `SETTLEMENT_DEPTH`, the other nodes broadcast the
/// block's settlement transaction on its behalf, and the settlement lands on BTC.
#[test]
fn offline_producers_settlement_is_broadcast_by_other_nodes() {
    use bitcoin::hashes::Hash;
    use common::net::wait as wait_for;
    use std::time::Duration;

    let (mut net, included, attached, bob) = offline_producer_setup("m5/offline1");
    net.nodes[0].node.take().unwrap().stop().unwrap();
    net.nodes[1].pause(false);
    wait_for(
        "the other nodes broadcast the settlement transaction",
        Duration::from_secs(60),
        || {
            net.mock
                .mempool()
                .iter()
                .any(|tx| tx.compute_txid().to_byte_array() == attached)
                .then_some(())
        },
    );
    net.nodes[1].pause(true);
    let by_others = net.nodes[1..].iter().any(|n| {
        n.broadcasts()
            .iter()
            .any(|tx| tx.compute_txid().to_byte_array() == attached)
    });
    assert!(by_others, "broadcast by an online node");
    net.mock.mine(1);
    let s = net.nodes[1].wait_settlement(included.height, SettlementState::Confirmed);
    assert_eq!(s.confirmed.unwrap().txid, attached);
    assert_eq!(net.nodes[2].ksa(&bob), 7);
}

/// The producer is offline and the block's settlement transaction is invalid (its funding input was spent elsewhere,
/// §7.3, D-045): after waiting 2 BTC blocks, the other nodes start a rescue settlement with their own funding (payload
/// unchanged), the rescue settlement lands on BTC, and the op remains effective.
#[test]
fn offline_producers_invalid_settlement_is_rescued_by_other_nodes() {
    use bitcoin::hashes::Hash;
    use common::net::wait as wait_for;
    use std::time::Duration;

    let (mut net, included, attached, bob) = offline_producer_setup("m5/offline2");
    net.nodes[0].node.take().unwrap().stop().unwrap();
    let point = spend_settlement_input(&net, included.height);
    // The other nodes produce blocks so the block reaches the depth; after finding the settlement transaction cannot be
    // broadcast, they wait another 2 BTC blocks
    net.nodes[1].pause(false);
    wait_for(
        "the block reaches the depth",
        Duration::from_secs(30),
        || (net.nodes[1].height() >= included.height + DEVNET.settlement_depth).then_some(()),
    );
    std::thread::sleep(Duration::from_millis(500));
    net.mock.mine(ksana_node::rescue::RESCUE_DELAY);
    // The rescue settlement's payload equals the original settlement transaction's (§7.3); an unlock block's
    // settlement may exist at the same time and must not be confused with it
    let payload = ksana_btc::payload::attached_payload(
        &DEVNET,
        &ksana_btc::settlement::SettlementPair::decode(
            net.nodes[1]
                .settlement(included.height)
                .txs
                .as_ref()
                .expect("the block carries a settlement transaction"),
        )
        .unwrap(),
    )
    .unwrap();
    let rescue = wait_for("rescue settlement", Duration::from_secs(60), || {
        net.mock.mempool().into_iter().find(|tx| {
            tx.compute_txid().to_byte_array() != attached
                && ksana_btc::payload::envelope_payload(&DEVNET, tx).as_ref() == Some(&payload)
        })
    });
    net.nodes[1].pause(true);
    // The rescue is a commit / reveal pair (D-081): its commit transaction does not spend the already spent funding
    // output
    let rescue_commit = net
        .mock
        .mempool()
        .into_iter()
        .find(|tx| tx.compute_txid() == rescue.input[0].previous_output.txid)
        .expect("the rescue's commit transaction");
    assert!(rescue_commit
        .input
        .iter()
        .all(|i| i.previous_output != point));
    let rescuers = net.nodes[1..]
        .iter()
        .filter(|n| {
            n.broadcasts()
                .iter()
                .any(|tx| tx.compute_txid() == rescue.compute_txid())
        })
        .count();
    assert!(rescuers >= 1, "the rescue is started by an online node");
    net.mock.mine(1);
    let s = net.nodes[1].wait_settlement(included.height, SettlementState::Confirmed);
    let confirmed = s.confirmed.unwrap().txid;
    assert_ne!(confirmed, attached);
    assert!(
        net.nodes[1..]
            .iter()
            .flat_map(|n| n.broadcasts())
            .any(|tx| {
                tx.compute_txid().to_byte_array() == confirmed
                    && ksana_btc::payload::envelope_payload(&DEVNET, &tx).as_ref() == Some(&payload)
            }),
        "what landed is the rescue settlement"
    );
    assert_eq!(net.nodes[1].ksa(&bob), 7, "the op remains effective");
}

/// Simulates bitcoind letting `sendrawtransaction` return only long after accepting the first `Headers` transaction
/// (Bitcoin Core returns only after the validation queue's callbacks are processed, which can be slow with many
/// wallets loaded); records every transaction checked (`testmempoolaccept`) or broadcast through it.
struct SlowHeaders {
    inner: std::sync::Arc<ksana_btc::mock::MockBtc>,
    delayed: std::sync::atomic::AtomicBool,
    seen: std::sync::Mutex<Vec<bitcoin::Transaction>>,
}

mod slow_headers {
    use bitcoin::{OutPoint, Script, Transaction};
    use ksana_btc::client::{BtcClient, BtcError, HeaderInfo, ScannedUtxo, TxInfo, UtxoInfo};
    use ksana_primitives::{Payload, DEVNET};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    impl BtcClient for super::SlowHeaders {
        fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
            self.inner.tip()
        }
        fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
            self.inner.block_hash(height)
        }
        fn block(&self, hash: &[u8; 32]) -> Result<Option<bitcoin::Block>, BtcError> {
            self.inner.block(hash)
        }
        fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
            self.inner.header(hash)
        }
        fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
            self.inner.transaction(txid)
        }
        fn utxo(
            &self,
            outpoint: &OutPoint,
            include_mempool: bool,
        ) -> Result<Option<UtxoInfo>, BtcError> {
            self.inner.utxo(outpoint, include_mempool)
        }
        fn scan_utxos(&self, script: &Script) -> Result<Vec<ScannedUtxo>, BtcError> {
            self.inner.scan_utxos(script)
        }
        fn test_accept(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
            self.seen
                .lock()
                .unwrap()
                .extend(txs.iter().map(|tx| (*tx).clone()));
            self.inner.test_accept(txs)
        }
        fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
            self.seen.lock().unwrap().push(tx.clone());
            let result = self.inner.broadcast(tx);
            let headers = matches!(
                ksana_btc::data::tx_payload(&DEVNET, tx),
                Some((_, Payload::Headers { .. }))
            );
            if headers && matches!(result, Ok(Ok(()))) && !self.delayed.swap(true, Ordering::SeqCst)
            {
                std::thread::sleep(Duration::from_secs(3));
            }
            result
        }
        fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
            self.inner.in_mempool(txid)
        }
        fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
            self.inner.mempool_txids()
        }
        fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
            self.inner.txindex_height()
        }
    }
}

/// While the republish transaction has entered bitcoind's mempool and the node's broadcast call has not returned yet,
/// the production loop picks funding for a new block's settlement transaction: it must not pick the republish
/// transaction's funding output (otherwise under full-RBF the settlement transaction would replace the republish, and
/// the node would then give up the republish and resend it). The funding of republish and rescue transactions is
/// reserved atomically before broadcasting (`Chain::try_reserve_inputs`, D-047); before the fix, the settlement
/// transaction was assembled here with the same funding output.
#[test]
fn a_republish_in_flight_keeps_its_funding_input_out_of_new_settlements() {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    use ksana_primitives::Payload;

    let (alice, bob) = (user("m5/alice7"), user("m5/bob7"));
    let mut slow = None;
    // Over 500 empty blocks are needed before headers are republished: the test algorithm is used to produce blocks
    // (see above)
    let n = MockNode::start_with_algorithm(
        &[(&alice, 1_000)],
        Duration::from_millis(10),
        |mock| {
            let wrapper = Arc::new(SlowHeaders {
                inner: mock,
                delayed: Default::default(),
                seen: Default::default(),
            });
            slow = Some(Arc::clone(&wrapper));
            wrapper
        },
        Arc::new(Sha256d),
    );
    let slow = slow.unwrap();
    let republish = wait("header republish transaction", || {
        n.mock.mempool().into_iter().find(|tx| {
            matches!(
                ksana_btc::data::tx_payload(&DEVNET, tx),
                Some((_, Payload::Headers { .. }))
            )
        })
    });
    // The node's broadcast call is still waiting to return: an op submitted now needs a settlement transaction
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    n.wait_included(&n.submit(&op));

    let spent: BTreeSet<_> = republish.input.iter().map(|i| i.previous_output).collect();
    let republish_txid = republish.compute_txid();
    for tx in slow.seen.lock().unwrap().iter() {
        if tx.compute_txid() == republish_txid {
            continue;
        }
        assert!(
            tx.input.iter().all(|i| !spent.contains(&i.previous_output)),
            "transaction {} spends the same funding output as the in-flight republish transaction",
            tx.compute_txid()
        );
    }
    assert!(
        n.mock
            .mempool()
            .iter()
            .any(|tx| tx.compute_txid() == republish_txid),
        "the republish transaction is still in the mempool"
    );
}

/// A funding output is spent by a transaction this node no longer tracks (e.g. a republish or rescue transaction in
/// flight before a restart, or another spend of the funding wallet) and that transaction is still in the mempool:
/// after bitcoind rejects the settlement transaction, the production loop removes this output from the funding cache
/// of this anchor and uses other funding instead, and the op is included as usual. Before the fix it kept picking the
/// same output until that transaction confirmed.
#[test]
fn a_funding_output_spent_in_the_mempool_is_skipped() {
    use bitcoin::absolute::LockTime;
    use bitcoin::transaction::Version;
    use bitcoin::{Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};
    use ksana_btc::client::BtcClient;

    let (alice, bob) = (user("m5/alice8"), user("m5/bob8"));
    let n = MockNode::start(&[(&alice, 1_000)]);
    // The funding UTXOs have the same amount and the producer picks the first in scan order: spend it in the mempool
    let first = n
        .mock
        .scan_utxos(&n.funding.script)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    n.mock
        .submit(Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: first.outpoint,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: first.txout.value - Amount::from_sat(1_000),
                script_pubkey: ksana_btc::mock::coinbase_script(),
            }],
        })
        .unwrap();
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let block = n.wait_included(&n.submit(&op)).block.unwrap();
    let pair =
        ksana_btc::settlement::SettlementPair::decode(&n.block(block.height).settlement.unwrap())
            .unwrap();
    assert!(
        pair.commit
            .input
            .iter()
            .all(|i| i.previous_output != first.outpoint),
        "the settlement transaction no longer spends the already spent funding output"
    );
}

/// Rescues proceed in fast-lane order and check the BTC mempool before starting (D-057): producer A is offline and its
/// funding input was spent elsewhere, B and C are both online. After the node ordered first sends its rescue (held in
/// the mempool, not landing for now), enough BTC blocks are mined for any node to finish waiting, and the other node,
/// seeing the block's settlement already in the mempool, does not start a second one.
#[test]
fn rescues_follow_the_fast_chain_order_and_skip_blocks_already_rescued_in_the_mempool() {
    use bitcoin::hashes::Hash;
    use common::net::wait as wait_for;
    use ksana_node::rescue::{RESCUE_DELAY, RESCUE_ORDER_WINDOW};
    use std::time::Duration;

    let (mut net, included, attached, bob) = offline_producer_setup("m6.1/order");
    net.nodes[0].node.take().unwrap().stop().unwrap();
    spend_settlement_input(&net, included.height);
    // Only B produces blocks so the block reaches the depth: B is ordered first in the rescue order and C is outside
    // the order window. If B and C produced simultaneously they would fork at the same height, each believing itself
    // ordered first, and rescue at the same time
    net.nodes[1].pause(false);
    wait_for(
        "the block reaches the depth",
        Duration::from_secs(30),
        || {
            net.nodes[1..]
                .iter()
                .all(|n| n.height() >= included.height + DEVNET.settlement_depth)
                .then_some(())
        },
    );
    let payload = ksana_btc::payload::attached_payload(
        &DEVNET,
        &ksana_btc::settlement::SettlementPair::decode(
            net.nodes[1]
                .settlement(included.height)
                .txs
                .as_ref()
                .expect("the block carries a settlement transaction"),
        )
        .unwrap(),
    )
    .unwrap();
    let is_rescue = |tx: &bitcoin::Transaction| {
        tx.compute_txid().to_byte_array() != attached
            && ksana_btc::payload::envelope_payload(&DEVNET, tx).as_ref() == Some(&payload)
    };
    let rescues = |net: &common::net::Net| -> std::collections::BTreeSet<bitcoin::Txid> {
        net.nodes[1..]
            .iter()
            .flat_map(|n| n.broadcasts())
            .filter(|tx| is_rescue(tx))
            .map(|tx| tx.compute_txid())
            .collect()
    };
    // Mine BTC blocks one at a time until the node ordered first sends its rescue; hold it so it does not land
    let first = wait_for("first rescue", Duration::from_secs(60), || {
        std::thread::sleep(Duration::from_millis(300));
        let found = rescues(&net).into_iter().next();
        if found.is_none() {
            net.mock.mine(1);
        }
        found
    });
    net.mock.hold(&first.to_byte_array());
    // Mine enough BTC blocks for any node to finish waiting: the other node sees the block's settlement already in the
    // mempool and does not start one
    for _ in 0..RESCUE_ORDER_WINDOW + RESCUE_DELAY + 1 {
        net.mock.mine(1);
        std::thread::sleep(Duration::from_millis(400));
    }
    net.nodes[1].pause(true);
    net.nodes[2].pause(true);
    assert_eq!(
        rescues(&net).len(),
        1,
        "only one rescue for the same block: {:?}",
        rescues(&net)
    );
    net.mock.release(&first.to_byte_array());
    net.mock.mine(1);
    let s = net.nodes[1].wait_settlement(included.height, SettlementState::Confirmed);
    assert_eq!(s.confirmed.unwrap().txid, first.to_byte_array());
    assert_eq!(net.nodes[2].ksa(&bob), 7, "the op remains effective");
}
