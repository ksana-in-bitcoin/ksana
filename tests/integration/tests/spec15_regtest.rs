//! Scenarios of the SPEC §15 scenario table that the other milestones' acceptance tests do not cover directly on
//! regtest (`tests/integration/run.sh`):
//!
//! - The same nonce via path B and path C at the same time: the one included first takes effect, the other is recorded
//!   as failed (path C is force-included but fails; path B after path C is no longer included, and the node removes it
//!   from the entry pool). The original scenario was path A and path C; path A has been removed (D-078).
//! - A 1-block BTC reorg of the block that confirmed a settlement: the settlement transaction lands on BTC again, and the
//!   block it settles, anchored below the reorg, stays on the main chain with its state (on devnet, whose
//!   `ANCHOR_DEPTH` is 0, the blocks anchored on the reorged-out BTC block leave the main chain, D-104).
//!
//! The other scenarios are in the milestones' acceptance tests (`m*_regtest.rs` and the crates' own tests).

mod common;

use std::time::Duration;

use bitcoin::hashes::Hash;
use common::*;
use ksana_integration_tests::Btc;
use ksana_primitives::{Entry, FeeMode};
use ksana_rpc::{KsanaApiClient, OpState, SettlementState};
use serde_json::json;

fn op_status(fast: &Fast, op_id: &[u8; 32]) -> Option<ksana_rpc::OpStatusJson> {
    fast.rt
        .block_on(fast.client.get_op_status(hex::encode(op_id)))
        .ok()
}

fn nonce(fast: &Fast, u: &User) -> u64 {
    fast.rt
        .block_on(fast.client.get_account(hex::encode(u.account)))
        .unwrap()
        .nonce
}

/// Sends a native transaction (path C) funded by the wallet and returns its op_id.
fn send_native(btc: &Btc, u: &User, nonce: u64, to: [u8; 32], amount: u128) -> [u8; 32] {
    let (_, op) = pay_native(btc, &|input| signed_native(u, nonce, input, to, amount));
    op.op_id().unwrap()
}

/// Mines BTC blocks while waiting for the op to be included.
fn mine_until_included(btc: &Btc, fast: &Fast, op_id: &[u8; 32]) -> ksana_rpc::OpStatusJson {
    wait("the op to be included", Duration::from_secs(120), || {
        mine(btc, 1);
        std::thread::sleep(Duration::from_millis(300));
        op_status(fast, op_id).filter(|s| s.status != OpState::Pending)
    })
}

#[test]
fn same_nonce_via_path_b_and_path_c_first_included_wins() {
    let btc = Btc::from_env().funded_wallet("ksana-it-spec15");
    let (alice, carol, bob) = (
        user("spec15/alice"),
        user("spec15/carol"),
        user("spec15/bob"),
    );
    let fast = Fast::start(&btc, "spec15/nonce", &[(&alice, 1_000), (&carol, 1_000)], 6);
    while fast.node().chain().sync_btc().unwrap() {}

    // 1. Path B is included first: the later native transaction with the same nonce is force-included but fails; the
    //    state is unchanged
    let path_b = signed(&alice, 0, FeeMode::Token { amount: 5 }, bob.account, 7);
    let b_id = fast.submit(&path_b).unwrap();
    assert_eq!(fast.wait_included(&b_id).status, OpState::FastConfirmed);
    let c_id = send_native(&btc, &alice, 0, bob.account, 3);
    let c = mine_until_included(&btc, &fast, &c_id);
    assert_eq!(
        c.status,
        OpState::Failed,
        "path C, included later, is recorded as failed"
    );
    assert_eq!(c.index, Some(0), "force-included at the start of the block");
    assert_eq!(fast.ksa(&bob), 7);
    assert_eq!(nonce(&fast, &alice), 1);

    // 2. Path C is included first: the path-B op with the same nonce is already in the entry pool but is no longer
    //    included, and the node removes it from the entry pool
    fast.node().set_paused(true);
    let c_id = send_native(&btc, &carol, 0, bob.account, 4);
    mine(&btc, 1);
    while fast.node().chain().sync_btc().unwrap() {}
    let path_b = signed(&carol, 0, FeeMode::Token { amount: 5 }, bob.account, 9);
    let b_id = fast.submit(&path_b).unwrap();
    fast.node().set_paused(false);
    let c = mine_until_included(&btc, &fast, &c_id);
    assert_eq!(
        c.status,
        OpState::FastConfirmed,
        "path C, included first, takes effect"
    );
    let first = c.block.unwrap().height;
    fast.wait_height(first + 3);
    assert!(
        op_status(&fast, &b_id).is_none_or(|s| s.status == OpState::Pending && s.block.is_none())
            && (first..=fast.tip_height()).all(|h| fast
                .block(h)
                .entries
                .iter()
                .all(|e| !matches!(e, Entry::Sponsored { op } if op.op_id().unwrap() == b_id))),
        "path B is no longer included"
    );
    wait(
        "path B to be removed from the entry pool",
        Duration::from_secs(30),
        || op_status(&fast, &b_id).is_none().then_some(()),
    );
    assert_eq!(fast.ksa(&bob), 7 + 4);
    assert_eq!(nonce(&fast, &carol), 1);
}

#[test]
fn btc_one_block_reorg_reconfirms_the_settlement_without_rolling_back() {
    let btc = Btc::from_env().funded_wallet("ksana-it-spec15");
    let (gina, hank) = (user("spec15/gina"), user("spec15/hank"));
    let fast = Fast::start(&btc, "spec15/reorg", &[(&gina, 1_000)], 6);
    let op = signed(&gina, 0, FeeMode::Token { amount: 3 }, hank.account, 5);
    let block = fast
        .wait_included(&fast.submit(&op).unwrap())
        .block
        .unwrap();
    let s = fast.wait_settlement(block.height, SettlementState::Broadcast);
    mine(&btc, 1);
    let confirmed = fast.wait_settlement(block.height, SettlementState::Confirmed);
    let confirmed_in = confirmed.confirmed.unwrap();
    let fast_height = fast.tip_height();

    // Reorg out the BTC block that confirmed the settlement: the settlement transaction returns to the mempool and
    // confirms again on the new chain; the block it settles, anchored below the reorg, does not roll back
    let reorged = bitcoin::BlockHash::from_byte_array(confirmed_in.block).to_string();
    btc.call("invalidateblock", &[json!(reorged)]);
    wait(
        "the settlement to return to the mempool",
        Duration::from_secs(30),
        || {
            let txid = bitcoin::Txid::from_byte_array(
                s.txid.expect("the block carries settlement transactions"),
            )
            .to_string();
            btc.try_call("getmempoolentry", &[json!(txid)]).ok()
        },
    );
    mine(&btc, 2);
    let again = wait("the reconfirmation", Duration::from_secs(60), || {
        let s = fast.settlement(block.height);
        s.confirmed
            .as_ref()
            .filter(|c| c.block != confirmed_in.block)
            .map(|_| s.clone())
    });
    assert_eq!(again.txid, s.txid, "the same settlement transaction");
    fast.wait_height(fast_height + 2);
    assert_eq!(
        fast.block(block.height).hash,
        block.hash,
        "the fast lane does not roll back"
    );
    assert_eq!(fast.ksa(&hank), 5);
    mine(&btc, 1);
    wait(
        "entry into the settled prefix",
        Duration::from_secs(60),
        || fast.settlement(block.height).settled.then_some(()),
    );
}
