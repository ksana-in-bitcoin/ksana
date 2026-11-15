//! The packing floor of path B follows the BTC fee rate at the operator's KSA price (D-153, local policy): with
//! `--ksa-price-sat` a KiB of an op's cost must pay the KSA worth the 256 vB it takes on BTC at the rate this node funds
//! settlements at; the producer sets the floor at each new BTC tip, ops below it wait in the pool, and
//! `ksana_estimateFee` suggests an amount that meets it. Without a price nothing changes.

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
fn the_floor_is_the_ksa_worth_256_vb_per_kib() {
    let fees = FeePolicy::new(2);
    assert_eq!(fees.path_b_floor(50), None, "no price, no floor");
    let fees = fees.with_ksa_price("1000".parse().unwrap());
    assert_eq!(PATH_B_VB_PER_KIB, 256);
    assert_eq!(fees.path_b_floor(2), Some(51_200_000));
    assert_eq!(fees.path_b_floor(1_000), Some(25_600_000_000));
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

    // No estimate from bitcoind: the node pays its floor of 2 sat/vB, so a KiB must pay 512 sat, 51 200 000 base
    // units at 1 000 sat a KSA
    let low = estimate();
    assert_eq!(low.feerate_sat_vb, 2);
    assert_eq!(low.path_b.per_kib, 51_200_000);
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

    // bitcoind estimates 10 sat/vB: `ksana_estimateFee` follows at once, the producer at the next BTC block
    n.mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    let high = estimate();
    assert_eq!(high.feerate_sat_vb, 10);
    assert_eq!(high.path_b.per_kib, 256_000_000);
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
    let waiting = n.submit(&transfer(1, low.path_b.amount));
    let height = n.tip_height();
    common::mock_node::wait("two more blocks", || {
        (n.tip_height() >= height + 2).then_some(())
    });
    assert_eq!(status(&waiting), OpState::Pending);

    // The estimate falls again: `ksana_estimateFee` keeps to the floor the producer still packs by, and the op that
    // waited is packed after the next BTC block
    n.mock.set_fee_estimate(DEFAULT_FEE_TARGET, None);
    let e = estimate();
    assert_eq!((e.feerate_sat_vb, e.path_b.per_kib), (2, 256_000_000));
    n.mock.mine(1);
    assert_eq!(n.wait_included(&waiting).status, OpState::FastConfirmed);
    let per_kib = estimate().path_b.per_kib;
    assert!((51_200_000..52_000_000).contains(&per_kib), "{per_kib}");
    assert_eq!(status(&cheap), OpState::Pending);
}
