//! M6.2 regtest acceptance (`tests/integration/run.sh`): the settlement funding wallet and the revenue wallet are
//! separate (D-060).
//!
//! The producer's hot wallet has 6 UTXOs of 0.002 BTC, the cap is 0.005 BTC, and the revenue address is a bech32
//! address of the test wallet: the first settlement uses only one funding input, and without it the hot wallet still
//! has 0.01 BTC, not below the cap, so the change is paid to the revenue address; bitcoind accepts and confirms this
//! transaction, and the revenue address receives the change.

mod common;

use std::str::FromStr;

use bitcoin::hashes::Hash;
use common::*;
use ksana_integration_tests::Btc;
use ksana_node::producer::Revenue;
use ksana_primitives::FeeMode;
use ksana_rpc::SettlementState;
use serde_json::json;

#[test]
fn settlement_change_pays_the_revenue_address_above_the_hot_wallet_cap() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m62");
    let (alice, bob) = (user("m62/alice"), user("m62/bob"));
    let address = btc.new_address();
    let revenue = bitcoin::Address::from_str(&address)
        .unwrap()
        .require_network(bitcoin::Network::Regtest)
        .unwrap()
        .script_pubkey();
    let fast = Fast::start_with_revenue(
        &btc,
        "m62/revenue",
        &[(&alice, 1_000)],
        6,
        Revenue {
            script: revenue.clone(),
            hot_cap_sat: 500_000,
        },
    );
    let op = signed(&alice, 0, FeeMode::Token { amount: 3 }, bob.account, 5);
    let height = fast
        .wait_included(&fast.submit(&op).unwrap())
        .block
        .unwrap()
        .height;
    let s = fast.wait_settlement(height, SettlementState::Broadcast);
    // The change is in the commit transaction (D-081)
    let pair = ksana_btc::settlement::SettlementPair::decode(
        s.txs
            .as_ref()
            .expect("the block carries settlement transactions"),
    )
    .unwrap();
    assert_eq!(pair.commit.input.len(), 1, "one funding input");
    assert_eq!(
        pair.commit.output[0].script_pubkey, revenue,
        "change paid to the revenue address"
    );
    mine(&btc, 1);
    fast.wait_settlement(height, SettlementState::Confirmed);

    // The revenue address (the test wallet) receives the confirmed change
    let txid =
        bitcoin::Txid::from_byte_array(s.commit_txid.expect("commit transaction")).to_string();
    let out = btc.call("gettxout", &[json!(txid), json!(0)]);
    assert_eq!(
        out["scriptPubKey"]["address"].as_str(),
        Some(address.as_str())
    );
    assert!(out["confirmations"].as_u64().unwrap() >= 1);
    assert_eq!(fast.ksa(&bob), 5);
}
