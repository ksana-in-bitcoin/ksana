//! M6.2 regtest acceptance (`tests/integration/run.sh`): the settlement funding wallet and the revenue wallet are
//! separate (D-060, D-142).
//!
//! The producer's hot wallet has 6 UTXOs of 0.002 BTC, the cap is 0.005 BTC, and the revenue address is a bech32
//! address of the test wallet: the settlement's change returns to the hot wallet, and the rescue thread sweeps the
//! 0.007 BTC above the cap (less the fee) to the revenue address in a transaction of its own (D-142), which bitcoind
//! accepts and confirms.

mod common;

use std::str::FromStr;

use common::*;
use ksana_integration_tests::Btc;
use ksana_node::producer::Revenue;
use ksana_primitives::FeeMode;
use ksana_rpc::SettlementState;
use serde_json::json;

#[test]
fn the_excess_above_the_hot_wallet_cap_is_swept_to_the_revenue_address() {
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
    let pair = ksana_btc::settlement::SettlementPair::decode(
        s.txs
            .as_ref()
            .expect("the block carries settlement transactions"),
    )
    .unwrap();
    assert_eq!(
        pair.reveal.output[0].script_pubkey, fast.funding.script,
        "change returned to the hot wallet"
    );
    mine(&btc, 1);
    fast.wait_settlement(height, SettlementState::Confirmed);

    // The revenue address (the test wallet) receives the confirmed sweep
    let mut received = 0.0;
    for _ in 0..60 {
        received = btc
            .call("getreceivedbyaddress", &[json!(address), json!(1)])
            .as_f64()
            .unwrap();
        if received > 0.0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        mine(&btc, 1);
    }
    assert!(received > 0.0, "the revenue address received the sweep");
    assert!(received <= 0.007, "the sweep keeps the cap: {received}");
    assert_eq!(fast.ksa(&bob), 5);
}
