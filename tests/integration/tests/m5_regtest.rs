//! M5 acceptance tests (SPEC §14 M5, `tests/integration/run.sh`): the devnet's regtest bitcoind + an in-process
//! `ksana-node` (and `ksana-indexer`).
//!
//! 1. The indexer connects only to bitcoind; the rebuilt `state_root` agrees with the fast-lane node at all settled
//!    heights.
//! 2. The settlement transaction is rebuilt and lands on BTC, and the op remains valid (formerly: a path-A user
//!    double-spends the funding input and the account records a fee debt; path A and fee debts have been removed,
//!    D-078): the funding input of the producer's settlement transaction is spent elsewhere before landing, and the
//!    producer rebuilds the settlement transaction with other funding (payload unchanged).
//! 3. After the producer goes offline, other nodes complete the broadcast or a rescue settlement.
//! 4. In the settlement timeout scenario the fast lane does not roll back; the fast path pauses and resumes
//!    automatically once the settlement lands on BTC.
//! 5. Recovery after a long stall: once the earlier headers land on BTC through a republish transaction, the indexer
//!    continues, with `state_root` agreeing with the fast-lane node.
//!
//! Each test uses its own funding secret key, users and data directory; only the test itself mines BTC blocks. Run in
//! order with `--test-threads=1`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use bitcoin::consensus::encode::deserialize_hex;
use bitcoin::hashes::Hash;
use bitcoin::{Transaction, Txid};
use common::*;
use ksana_btc::data::tx_payload;
use ksana_btc::settlement::SettlementPair;
use ksana_consensus::Sha256d;
use ksana_integration_tests::Btc;
use ksana_primitives::{Action, Entry, FeeMode, Payload, DEVNET};
use ksana_rpc::{KsanaApiClient, OpState, SettlementState};
use serde_json::json;

fn txid(bytes: [u8; 32]) -> Txid {
    Txid::from_byte_array(bytes)
}

fn in_mempool(btc: &Btc, txid: &Txid) -> bool {
    btc.try_call("getmempoolentry", &[json!(txid.to_string())])
        .is_ok()
}

/// The transactions in bitcoind's mempool.
fn mempool_txs(btc: &Btc) -> Vec<Transaction> {
    let ids = btc.call("getrawmempool", &[]);
    ids.as_array()
        .unwrap()
        .iter()
        .filter_map(|id| {
            let hex = btc
                .try_call("getrawtransaction", std::slice::from_ref(id))
                .ok()?;
            deserialize_hex(hex.as_str()?).ok()
        })
        .collect()
}

/// The op's status; `None` while the node does not know it yet (before a native transaction is included).
fn op_status(fast: &Fast, op_id: &[u8; 32]) -> Option<ksana_rpc::OpStatusJson> {
    fast.rt
        .block_on(fast.client.get_op_status(hex::encode(op_id)))
        .ok()
}

/// Mines BTC blocks while waiting, until a block enters the node's settled prefix (its settlement transaction has
/// landed on BTC with enough confirmations).
fn mine_until_settled(btc: &Btc, fast: &Fast, height: u64) {
    wait(
        "entry into the settled prefix",
        Duration::from_secs(120),
        || {
            mine(btc, 1);
            std::thread::sleep(Duration::from_millis(300));
            fast.settlement(height).settled.then_some(())
        },
    );
}

/// Acceptance 4: the settlement transaction fails to land on BTC (mining priority lowered to simulate a fee spike) for
/// more than `SETTLEMENT_TIMEOUT` BTC blocks: the fast lane does not roll back, later blocks contain only
/// forced-inclusion entries and new ops stay in the entry pool; once the settlement lands, the fast path resumes
/// automatically.
#[test]
fn settlement_timeout_pauses_the_fast_path_until_the_settlement_confirms() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m5");
    let (alice, bob) = (user("timeout/alice"), user("timeout/bob"));
    let fast = Fast::start(&btc, "m5/timeout", &[(&alice, 1_000)], 6);
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let stuck = fast
        .wait_included(&fast.submit(&op).unwrap())
        .block
        .unwrap();
    let s = fast.wait_settlement(stuck.height, SettlementState::Broadcast);
    let stuck_txid = txid(s.txid.expect("the block carries settlement transactions"));
    // Hold back both the commit and the reveal (D-081)
    let stuck_txids = [txid(s.commit_txid.expect("commit transaction")), stuck_txid];
    for t in &stuck_txids {
        btc.call(
            "prioritisetransaction",
            &[json!(t.to_string()), json!(0), json!(-100_000)],
        );
    }
    let timeout_at = fast.block(stuck.height).header.btc_anchor.height + DEVNET.settlement_timeout;
    while btc_tip(&btc).height < timeout_at {
        mine(&btc, 1);
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        in_mempool(&btc, &stuck_txid),
        "the settlement transaction is still stuck in the mempool"
    );
    let first_paused = wait(
        "a block anchored at the timeout height",
        Duration::from_secs(60),
        || {
            let tip = fast.tip_height();
            (fast.block(tip).header.btc_anchor.height >= timeout_at).then_some(tip)
        },
    );
    let op2 = signed(&alice, 1, FeeMode::Token { amount: 2 }, bob.account, 6);
    let op2_id = fast.submit(&op2).unwrap();
    fast.wait_height(first_paused + 3);
    for height in first_paused..=fast.tip_height() {
        let block = fast.block(height);
        if block.header.btc_anchor.height >= timeout_at {
            assert!(
                block
                    .entries
                    .iter()
                    .all(|e| matches!(e, Entry::Forced { .. })),
                "the block at height {height} contains fast-path entries"
            );
        }
    }
    assert_eq!(fast.status(&op2_id).status, OpState::Pending);
    assert_eq!(
        fast.block(stuck.height).hash,
        stuck.hash,
        "the fast lane does not roll back"
    );

    // Restore the priority; once the settlement lands, the fast path resumes automatically
    for t in &stuck_txids {
        btc.call(
            "prioritisetransaction",
            &[json!(t.to_string()), json!(0), json!(100_000)],
        );
    }
    mine(&btc, 1);
    fast.wait_settlement(stuck.height, SettlementState::Confirmed);
    assert_eq!(fast.wait_included(&op2_id).status, OpState::FastConfirmed);
    assert_eq!(fast.ksa(&bob), 11);
}

/// Acceptance 2 (revised by D-078): the funding input of the producer's settlement transaction is spent elsewhere
/// before the broadcast (the conflicting transaction lands on BTC): once the block has depth, the original settlement
/// transaction cannot be broadcast, and the producer (for its own block) immediately rebuilds it with other funding,
/// payload unchanged; the rebuilt settlement lands on BTC and the op remains valid. The indexer recognizes the rebuilt
/// settlement by the same rules.
#[test]
fn spent_settlement_input_is_rescued_by_the_producer() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m5");
    let (alice, bob) = (user("rescue/alice"), user("rescue/bob"));
    let fast = Fast::start(&btc, "m5/rescue", &[(&alice, 1_000)], 6);
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 7);
    let included = fast
        .wait_included(&fast.submit(&op).unwrap())
        .block
        .unwrap();
    // Pause block production: the block lacks depth and the settlement transaction is not broadcast yet
    fast.node().set_paused(true);
    assert!(fast.tip_height() < included.height + DEVNET.settlement_depth);
    let settlement = fast.settlement(included.height);
    let original = txid(
        settlement
            .txid
            .expect("the block carries settlement transactions"),
    );
    let attached = SettlementPair::decode(
        settlement
            .txs
            .as_ref()
            .expect("the block carries settlement transactions"),
    )
    .unwrap();
    let payload = ksana_btc::payload::attached_payload(&DEVNET, &attached).unwrap();

    // This output at the funding address is spent elsewhere; the conflicting transaction lands on BTC
    let point = attached.commit.input[0].previous_output;
    spend_producer_input(&btc, &fast.funding.keypair, point);
    mine(&btc, 1);

    // Resume block production: once the block has depth, the original settlement transaction cannot be broadcast and
    // the producer rebuilds it immediately; the rebuilt settlement lands on BTC
    fast.node().set_paused(false);
    let s = wait(
        "the rescue settlement to land on BTC",
        Duration::from_secs(90),
        || {
            std::thread::sleep(Duration::from_millis(500));
            mine(&btc, 1);
            let s = fast.settlement(included.height);
            (s.status == SettlementState::Confirmed).then_some(s)
        },
    );
    let confirmed = txid(s.confirmed.unwrap().txid);
    assert_ne!(
        confirmed, original,
        "the rebuilt settlement transaction is the one that landed"
    );
    // The rebuilt reveal transaction landed; the commit transaction holding the commitment output it spends no longer
    // spends the occupied output (D-081)
    let (tx, _) = btc.get_confirmed_tx(&confirmed);
    let (commit, _) = btc.get_confirmed_tx(&tx.input[0].previous_output.txid);
    assert!(
        commit.input.iter().all(|i| i.previous_output != point),
        "the rebuilt settlement no longer spends the occupied output"
    );
    assert_eq!(
        ksana_btc::payload::envelope_payload(&DEVNET, &tx),
        Some(payload),
        "payload unchanged"
    );
    assert_eq!(fast.ksa(&bob), 7, "the op remains valid");

    // After the block enters the settled prefix, the indexer's state_root at every height agrees with the node
    settle_for_indexer(&btc, &fast, included.height);
    let (ix, _dir) = indexer(&fast);
    index_and_compare(&ix, included.height, |h| fast.block(h));
}

/// 3 nodes; producer A includes a path-B op and pauses immediately (the block lacks depth before A goes offline, so
/// A did not broadcast); returns the network, the block and the settlement transactions it carries.
fn offline_setup(btc: &Btc, name: &str, bob: &User) -> (Net, ksana_rpc::BlockRef, SettlementPair) {
    let alice = user(&format!("{name}/alice"));
    let net = Net::start(btc, name, &[(&alice, 1_000)], Duration::from_millis(300));
    net.mine_on(0, 1);
    net.converge();
    for node in &net.nodes {
        while node.node().chain().sync_btc().unwrap() {}
    }
    let a = &net.nodes[0];
    let op_id = a.submit(&signed(
        &alice,
        0,
        FeeMode::Token { amount: 2 },
        bob.account,
        7,
    ));
    a.pause(false);
    let included = wait("A to include the op", Duration::from_secs(60), || {
        a.status(&op_id)
            .filter(|s| s.status == OpState::FastConfirmed)
            .and_then(|s| s.block)
    });
    a.pause(true);
    wait(
        "the other nodes to receive the block",
        Duration::from_secs(60),
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
        a.height() < included.height + DEVNET.settlement_depth,
        "the block lacks depth before A goes offline"
    );
    let attached = SettlementPair::decode(
        a.settlement(included.height)
            .txs
            .as_ref()
            .expect("the block carries settlement transactions"),
    )
    .unwrap();
    (net, included, attached)
}

/// Acceptance 3 (broadcast): producer A goes offline right after including the op: once the block has
/// `SETTLEMENT_DEPTH`, the other nodes broadcast the settlement transaction carried by the block on its behalf, and
/// the settlement lands on BTC.
#[test]
fn offline_producers_settlement_is_broadcast_by_other_nodes() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m5");
    let bob = user("offline/bob");
    let (mut net, included, attached) = offline_setup(&btc, "m5/offline", &bob);
    let attached_txid = attached.reveal.compute_txid();
    let commit_txid = attached.commit.compute_txid();
    net.nodes[0].node.take().unwrap().stop().unwrap();

    net.nodes[1].pause(false);
    wait(
        "the broadcast on A's behalf",
        Duration::from_secs(60),
        || in_mempool(&btc, &attached_txid).then_some(()),
    );
    net.nodes[1].pause(true);
    assert!(
        in_mempool(&btc, &commit_txid),
        "the commit transaction is broadcast too"
    );
    assert!(
        net.nodes[1..]
            .iter()
            .flat_map(|n| n.broadcasts())
            .any(|tx| tx.compute_txid() == attached_txid),
        "broadcast by an online node"
    );
    mine(&btc, 1);
    let s = net.nodes[1].wait_settlement(included.height, SettlementState::Confirmed);
    assert_eq!(txid(s.confirmed.unwrap().txid), attached_txid);
    assert_eq!(net.nodes[2].ksa(&bob), 7);
}

/// Acceptance 3 (rescue): producer A goes offline right after including the op, and the settlement transaction
/// carried by the block is invalid (its funding input was spent elsewhere): the other nodes start a rescue settlement
/// in fast-lane order (D-057: the producer of the k-th block after that block waits k BTC blocks after finding it
/// cannot be broadcast; payload unchanged, with its own funding), the rescue settlement lands on BTC, and the op
/// remains valid.
#[test]
fn offline_producers_invalid_settlement_is_rescued_by_other_nodes() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m5");
    let bob = user("rescued/bob");
    let (mut net, included, attached) = offline_setup(&btc, "m5/rescued", &bob);
    let attached_txid = attached.reveal.compute_txid();
    let payload = ksana_btc::payload::attached_payload(&DEVNET, &attached).unwrap();
    net.nodes[0].node.take().unwrap().stop().unwrap();

    // This output at A's funding address is spent elsewhere and the conflicting transaction lands on BTC: the
    // settlement transaction carried by A's block is invalid
    let point = attached.commit.input[0].previous_output;
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let a_key =
        bitcoin::secp256k1::Keypair::from_secret_key(&secp, &btc_secret("m5/rescued/funding/0"));
    spend_producer_input(&btc, &a_key, point);
    mine(&btc, 1);

    // B keeps producing blocks so that the block gets depth; the rescue may already land during the mining below, so
    // it is looked up by "bitcoind knows this transaction"
    net.nodes[1].pause(false);
    wait(
        "the block to have enough depth",
        Duration::from_secs(60),
        || (net.nodes[1].height() >= included.height + DEVNET.settlement_depth).then_some(()),
    );
    for _ in 0..ksana_node::rescue::RESCUE_DELAY {
        std::thread::sleep(Duration::from_millis(500));
        mine(&btc, 1);
    }
    // The rescue is a commit / reveal pair funded by this node (D-081): the reveal transaction carries the same
    // payload and its commit transaction does not spend the occupied output
    let sent = |net: &Net| -> Vec<Transaction> {
        net.nodes[1..].iter().flat_map(|n| n.broadcasts()).collect()
    };
    let is_rescue = |tx: &Transaction, sent: &[Transaction]| {
        tx.compute_txid() != attached_txid
            && ksana_btc::payload::envelope_payload(&DEVNET, tx).as_ref() == Some(&payload)
            && sent
                .iter()
                .filter(|c| c.compute_txid() == tx.input[0].previous_output.txid)
                .all(|c| c.input.iter().all(|i| i.previous_output != point))
    };
    let rescues = wait(
        "the rescue settlement to be broadcast",
        Duration::from_secs(60),
        || {
            let all = sent(&net);
            let rescues: Vec<Txid> = all
                .iter()
                .filter(|tx| is_rescue(tx, &all))
                .map(|tx| tx.compute_txid())
                .filter(|t| {
                    btc.try_call("getrawtransaction", &[json!(t.to_string())])
                        .is_ok()
                })
                .collect();
            (!rescues.is_empty()).then_some(rescues)
        },
    );
    net.nodes[1].pause(true);
    mine(&btc, 1);
    let s = net.nodes[1].wait_settlement(included.height, SettlementState::Confirmed);
    let confirmed = txid(s.confirmed.unwrap().txid);
    let all = sent(&net);
    assert!(
        all.iter()
            .any(|tx| tx.compute_txid() == confirmed && is_rescue(tx, &all)),
        "the rescue settlement {confirmed} is the one that landed (seen {rescues:?})"
    );
    assert_eq!(net.nodes[2].ksa(&bob), 7, "the op remains valid");
}

/// Acceptance 1: the indexer connects only to bitcoind (a fresh data directory, the same genesis config as the node),
/// and the rebuilt `state_root` agrees with the fast-lane node at all settled heights.
/// The chain has path-B and path-C ops, a contract deployment and call, blocks with only forced-inclusion entries,
/// empty blocks and unlock entries; afterwards the node keeps producing blocks, the same indexer syncs incrementally,
/// and the check is repeated.
#[test]
fn indexer_rebuilds_the_state_roots_of_all_settled_heights() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m5");
    let (alice, bob, carol) = (
        user("indexer/alice"),
        user("indexer/bob"),
        user("indexer/carol"),
    );
    let fast = Fast::start(
        &btc,
        "m5/indexer",
        &[(&alice, 1_000_000), (&carol, 1_000)],
        6,
    );
    let token = ksana_test_contracts::TOKEN;
    let init = borsh::to_vec(&("Index".to_string(), "IDX".to_string(), 0u8, 1_000u128)).unwrap();
    let token_id = ksana_execution::builtin::contract_id(
        &alice.account,
        2,
        &ksana_primitives::hash::sha256(token),
    );
    let ops = [
        signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5),
        signed(&alice, 1, FeeMode::Token { amount: 1 }, bob.account, 6),
        signed_action(
            &alice,
            2,
            FeeMode::Token { amount: 3 },
            Action::Deploy {
                code: token.to_vec(),
                init_args: init,
                funds: Vec::new(),
            },
            50_000_000,
        ),
        signed_action(
            &alice,
            3,
            FeeMode::Token { amount: 4 },
            Action::Call {
                contract: token_id,
                method: "transfer".into(),
                args: borsh::to_vec(&(bob.account, 250u128)).unwrap(),
                funds: Vec::new(),
            },
            5_000_000,
        ),
    ];
    let mut ids: Vec<[u8; 32]> = ops.iter().map(|op| fast.submit(op).unwrap()).collect();
    // Path C: carol's native transaction, included at the start of some block
    let (_, native) = pay_native(&btc, &|input| {
        signed_native(&carol, 0, input, bob.account, 3)
    });
    ids.push(native.op_id().unwrap());

    let mut last = 0;
    for id in &ids {
        let status = wait("the op to be included", Duration::from_secs(120), || {
            mine(&btc, 1);
            std::thread::sleep(Duration::from_millis(300));
            op_status(&fast, id).filter(|s| s.status != OpState::Pending)
        });
        assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
        last = last.max(status.block.unwrap().height);
    }
    assert_eq!(fast.ksa(&bob), 5 + 6 + 3);
    settle_for_indexer(&btc, &fast, last);
    let (ix, _dir) = indexer(&fast);
    let first = index_and_compare(&ix, last, |h| fast.block(h));

    // After the producer's token fees are unlocked (the block with the unlock entry also enters the settled prefix),
    // the same indexer syncs incrementally and the check is repeated
    let unlock_height = wait("the unlock entry", Duration::from_secs(120), || {
        mine(&btc, 1);
        std::thread::sleep(Duration::from_millis(300));
        (last + 1..=fast.tip_height()).find(|h| {
            fast.block(*h)
                .entries
                .iter()
                .any(|e| matches!(e, Entry::Unlock { .. }))
        })
    });
    // The first check may already have passed the unlock block (it settled while waiting for SETTLEMENT_DEPTH
    // confirmations): the target is at least one block above the first, making sure the sync is incremental
    let target = unlock_height.max(first.height + 1);
    settle_for_indexer(&btc, &fast, target);
    let second = index_and_compare(&ix, target, |h| fast.block(h));
    assert!(second.height > first.height);
}

/// Acceptance 5: recovery after a long stall. More than `MAX_SETTLEMENT_HEADERS` empty blocks in a row follow a
/// settled block, so the later block carrying a settlement transaction has no room for the earlier headers; while the
/// republish transaction the node sends for them is held back in the mempool (mining priority lowered), the indexer
/// stops before the empty blocks (its main-chain tip is the block before the empty blocks, up to the block with
/// `SETTLEMENT_DEPTH` confirmations, D-063), even though the later blocks' settlements have landed on BTC; once the
/// republish transaction lands, the indexer passes the empty blocks and continues, with the `state_root` at every
/// height agreeing with the node (§7.1, D-047).
///
/// Since block rewards (D-074), every time a settlement lands and the block enters the settled closure, the next block
/// writes an unlock entry (and thus carries a settlement transaction). So here the unlock block's settlement is first
/// left in the mempool (no BTC block mined) while enough empty blocks accumulate, and then one BTC block is mined: the
/// unlock block enters the settled closure, and the next unlock block's settlement payload can carry only the most
/// recent headers.
#[test]
fn indexer_continues_after_the_missing_headers_are_republished() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m5");
    let (alice, bob) = (user("gap/alice"), user("gap/bob"));
    // Over 500 empty blocks in a row are needed: produce them with the test algorithm (once the empty-block streak
    // weight reaches 120, Argon2id takes about 0.25 seconds per block, D-094); the indexer uses the same algorithm
    let fast = Fast::start_with_algorithm(
        &btc,
        "m5/gap",
        &[(&alice, 1_000)],
        6,
        Duration::from_millis(30),
        Arc::new(Sha256d),
    );
    let op1 = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let paid = fast
        .wait_included(&fast.submit(&op1).unwrap())
        .block
        .unwrap()
        .height;
    mine_until_settled(&btc, &fast, paid);
    let unlocks = |h: u64| {
        fast.block(h)
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Unlock { .. }))
    };
    // The producer's token fees and block reward are then unlocked: the unlock block carries a settlement
    // transaction, and the empty blocks start after it (no BTC block is mined, so no new unlockable records)
    let before = wait("the unlock block", Duration::from_secs(60), || {
        (paid + 1..=fast.tip_height()).find(|h| unlocks(*h))
    });
    let gap_end = before + DEVNET.max_settlement_headers as u64 + 5;
    fast.wait_height(gap_end);
    assert!((before + 1..=gap_end).all(|h| fast.block(h).entries.is_empty()));

    // The unlock block's settlement lands on BTC: it enters the settled closure, the empty blocks after it have no
    // data, and the node sends a republish transaction for them; hold it back
    fast.wait_settlement(before, SettlementState::Broadcast);
    mine(&btc, 1);
    fast.wait_settlement(before, SettlementState::Confirmed);
    let headers_tx = wait(
        "the header republish transaction",
        Duration::from_secs(120),
        || {
            mempool_txs(&btc).into_iter().find(|tx| {
                matches!(tx_payload(&DEVNET, tx), Some((_, Payload::Headers { headers, .. }))
                if headers.first().is_some_and(|h| h.height == before + 1))
            })
        },
    );
    let headers_txid = headers_tx.compute_txid();
    btc.call(
        "prioritisetransaction",
        &[
            json!(headers_txid.to_string()),
            json!(0),
            json!(-100_000_000),
        ],
    );
    let Some((_, Payload::Headers { headers, .. })) = tx_payload(&DEVNET, &headers_tx) else {
        unreachable!()
    };
    let covered = headers.last().unwrap().height;

    // The next unlock block after the empty blocks carries a settlement transaction (with only the most recent
    // headers); it landed on BTC, but the earlier headers still have not
    let after = wait("the next unlock block", Duration::from_secs(60), || {
        (gap_end + 1..=fast.tip_height()).find(|h| unlocks(*h))
    });
    assert!(
        after - before > DEVNET.max_settlement_headers as u64,
        "the settlement transaction has no room for all empty blocks' headers"
    );
    assert!(
        after - (DEVNET.max_settlement_headers as u64 - 1) <= covered + 1,
        "no gap between the republish transaction and the settlement transaction"
    );
    let s = wait(
        "the settlement transaction to land on BTC",
        Duration::from_secs(120),
        || {
            mine(&btc, 1);
            std::thread::sleep(Duration::from_millis(300));
            let s = fast.settlement(after);
            (s.status == SettlementState::Confirmed).then_some(s)
        },
    );
    assert!(
        !s.settled,
        "the earlier headers have not landed; the block is not in the settled prefix"
    );
    assert!(
        in_mempool(&btc, &headers_txid),
        "the republish transaction is still in the mempool"
    );

    // The indexer stops before the empty blocks
    let (ix, _dir) = indexer(&fast);
    let tip = btc_tip(&btc).height;
    let stalled = wait(
        "the indexer to scan up to the BTC tip",
        Duration::from_secs(60),
        || {
            let status = index_once(&ix);
            (status.btc_height >= Some(tip)).then_some(status)
        },
    );
    let confirmed = before - DEVNET.settlement_depth;
    assert_eq!(
        stalled.height, confirmed,
        "the indexer stops before the empty blocks"
    );
    assert_eq!(stalled.hash, fast.block(confirmed).hash);

    // Once the republish transaction lands, the indexer passes the empty blocks and continues, with the state_root
    // agreeing with the node; the node's settled prefix resumes advancing as well
    btc.call(
        "prioritisetransaction",
        &[
            json!(headers_txid.to_string()),
            json!(0),
            json!(100_000_000),
        ],
    );
    settle_for_indexer(&btc, &fast, after);
    let status = index_and_compare(&ix, after, |h| fast.block(h));
    assert!(status.height >= after);
}
