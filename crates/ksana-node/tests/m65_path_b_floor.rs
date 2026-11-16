//! The packing floor of path B follows the BTC fee rate at the operator's KSA price (D-153, local policy): with
//! `--ksa-price-sat` a KiB of an op's encoded bytes must pay the KSA worth the 128 vB it takes on BTC at the rate this
//! node funds settlements at, whatever the op's `max_gas`; the producer sets the floor at each new BTC tip, ops below
//! it wait in the pool, and `ksana_estimateFee` gives the rate and suggests an amount that meets it. Without a price
//! nothing changes, and the answer of `ksana_estimateFee` has the fields it had.

mod common;

use std::time::Duration;

use common::mock_node::{signed, MockNode};
use common::user;
use ksana_btc::client::BtcClient;
use ksana_node::producer::{FeePolicy, KsaPrice, DEFAULT_FEE_TARGET, PATH_B_VB_PER_KIB};
use ksana_primitives::FeeMode;
use ksana_rpc::{KsanaApiClient, OpState};

#[test]
fn a_ksa_price_is_a_positive_decimal_number() {
    let price = |s: &str| s.parse::<KsaPrice>();
    // One KSA (10^8 base units) worth 1 000 sat: a sat is 100 000 base units
    assert_eq!(price("1000").unwrap().base_units(1), 100_000);
    assert_eq!(price("1000.0").unwrap().base_units(512), 51_200_000);
    assert_eq!(price("0.5").unwrap().base_units(1), 200_000_000);
    assert_eq!(price("0.00000001").unwrap().base_units(1), 10_u128.pow(16));
    // Rounded up, so that the floor never asks for less than the cost
    assert_eq!(price("3").unwrap().base_units(1), 33_333_334);
    assert_eq!(price("3").unwrap().base_units(0), 0);
    // Saturating
    assert_eq!(
        price("0.00000001").unwrap().base_units(u128::MAX),
        u128::MAX
    );
    for bad in [
        "",
        "0",
        "0.0",
        ".5",
        "1.",
        "-1",
        "+1",
        "1e3",
        "1,5",
        " 1",
        "1.000000001",
        "1.2.3",
        "340282366920938463463374607431768211456",
    ] {
        assert!(price(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn the_floor_is_the_ksa_worth_128_vb_per_kib() {
    let fees = FeePolicy::new(2);
    assert_eq!(fees.path_b_floor(50), None, "no price, no floor");
    let fees = fees.with_ksa_price("1000".parse().unwrap());
    assert_eq!(PATH_B_VB_PER_KIB, 128);
    assert_eq!(fees.path_b_floor(2), Some(25_600_000));
    assert_eq!(fees.path_b_floor(1_000), Some(12_800_000_000));
    assert_eq!(fees.path_b_floor(0), Some(0));
}

#[test]
fn the_producer_packs_path_b_from_the_btc_fee_rate_on() {
    let (alice, bob, carol) = (user("m65/alice"), user("m65/bob"), user("m65/carol"));
    let n = MockNode::start_custom(
        &[(&alice, 100_000_000_000), (&carol, 100_000_000_000)],
        Duration::ZERO,
        |mock| mock,
        |funding| funding.with_fees(FeePolicy::new(2).with_ksa_price("1000".parse().unwrap())),
    );
    let estimate = || n.rt.block_on(n.client.estimate_fee()).unwrap();
    let status = |id: &[u8; 32]| {
        n.rt.block_on(n.client.get_op_status(hex::encode(id)))
            .unwrap()
            .status
    };
    let transfer =
        |nonce: u64, amount: u128| signed(&alice, nonce, FeeMode::Token { amount }, bob.account, 1);

    // No estimate from bitcoind: the node pays its floor of 2 sat/vB, so a KiB must pay 256 sat, 25 600 000 base
    // units at 1 000 sat a KSA
    let low = estimate();
    assert_eq!(low.feerate_sat_vb, 2);
    assert_eq!(
        (low.path_b.per_kib, low.path_b.per_kib_data),
        (0, 25_600_000)
    );
    assert!(low.path_b.amount > 1);
    // The suggested amount is packed, one base unit less is not
    let fee = FeeMode::Token {
        amount: low.path_b.amount - 1,
    };
    let cheap = n.submit(&signed(&carol, 0, fee, bob.account, 1));
    let height = n.tip_height();
    common::mock_node::wait("two more blocks", || {
        (n.tip_height() >= height + 2).then_some(())
    });
    assert_eq!(status(&cheap), OpState::Pending);
    let paid = n.submit(&transfer(0, low.path_b.amount));
    assert_eq!(n.wait_included(&paid).status, OpState::FastConfirmed);
    assert_eq!(status(&cheap), OpState::Pending);
    // The floor goes by the op's bytes: one that may use 30 times the gas, with a cost in the packing budget many
    // times its bytes, is packed for the same amount
    let mut thirsty = transfer(1, low.path_b.amount);
    thirsty.max_gas *= 30;
    thirsty.sig = ksana_crypto::sign_vmop(&thirsty, &alice.kp, &[2; 32]).unwrap();
    let len = thirsty.encode().unwrap().len();
    let rules = n.node().chain().params().rules(0);
    let cost = ksana_mempool::op_cost(n.node().chain().params(), &rules, len, thirsty.max_gas);
    assert!(cost > 5 * u64::try_from(len).unwrap(), "{cost} {len}");
    let thirsty = n.submit(&thirsty);
    assert_eq!(n.wait_included(&thirsty).status, OpState::FastConfirmed);

    // bitcoind estimates 10 sat/vB: `ksana_estimateFee` follows at once, the producer at the next BTC block
    n.mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    let high = estimate();
    assert_eq!(high.feerate_sat_vb, 10);
    assert_eq!(high.path_b.per_kib_data, 128_000_000);
    assert!(high.path_b.amount > low.path_b.amount * 4);
    n.mock.mine(1);
    common::mock_node::wait("the node to see the BTC block", || {
        (n.node().chain().btc().snapshot().ok()?.tip()? == n.mock.tip().unwrap()).then_some(())
    });
    let height = n.tip_height();
    common::mock_node::wait("two more blocks", || {
        (n.tip_height() >= height + 2).then_some(())
    });
    // What was enough before now waits in the pool
    let waiting = n.submit(&transfer(2, low.path_b.amount));
    let height = n.tip_height();
    common::mock_node::wait("two more blocks", || {
        (n.tip_height() >= height + 2).then_some(())
    });
    assert_eq!(status(&waiting), OpState::Pending);

    // The estimate falls again: `ksana_estimateFee` keeps to the floor the producer still packs by, and the op that
    // waited is packed after the next BTC block
    n.mock.set_fee_estimate(DEFAULT_FEE_TARGET, None);
    let e = estimate();
    assert_eq!((e.feerate_sat_vb, e.path_b.per_kib_data), (2, 128_000_000));
    n.mock.mine(1);
    assert_eq!(n.wait_included(&waiting).status, OpState::FastConfirmed);
    assert_eq!(estimate().path_b.per_kib_data, 25_600_000);
    assert_eq!(status(&cheap), OpState::Pending);
}

/// Without a KSA price `ksana_estimateFee` answers with the fields it had before D-153: `per_kib_data` is left out, so
/// clients that refuse unknown fields keep reading it.
#[test]
fn without_a_price_the_estimate_has_no_rate_per_kib_of_bytes() {
    let alice = user("m65/alice");
    let n = MockNode::start(&[(&alice, 1_000)]);
    let e = n.rt.block_on(n.client.estimate_fee()).unwrap();
    assert_eq!(e.path_b.per_kib_data, 0);
    let json = serde_json::to_value(&e.path_b).unwrap();
    let fields: Vec<&String> = json.as_object().unwrap().keys().collect();
    assert_eq!(fields, ["amount", "per_kib"]);
    let with: ksana_rpc::PathBEstimateJson =
        serde_json::from_str(r#"{"amount":"3","per_kib":"1","per_kib_data":"2"}"#).unwrap();
    assert_eq!((with.amount, with.per_kib, with.per_kib_data), (3, 1, 2));
    assert!(serde_json::to_string(&with)
        .unwrap()
        .contains("per_kib_data"));
}
