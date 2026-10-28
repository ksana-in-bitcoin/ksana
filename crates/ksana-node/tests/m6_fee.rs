//! `ksana_estimateFee` (SPEC §8.2, §11.2, D-054; the path A part was removed along with path A, D-078): for path B,
//! take the median over recent blocks of each block's lowest path-B entry fee rate (sponsored fee per KiB of cost); the
//! suggested amount is that of an ordinary transfer converted from it; also give the fee rate of this node's settlement
//! funding (bitcoind's estimate within the floor and the cap, the floor while bitcoind's mempool has room, D-102,
//! D-117).

mod common;

use common::mock_node::{signed, MockNode};
use common::user;
use ksana_btc::client::MempoolInfo;
use ksana_node::producer::DEFAULT_FEE_TARGET;
use ksana_primitives::FeeMode;
use ksana_rpc::KsanaApiClient;

#[test]
fn estimate_fee_follows_recent_path_b_amounts() {
    let (alice, bob) = (user("m6/fee/alice"), user("m6/fee/bob"));
    let n = MockNode::start(&[(&alice, 1_000)]);
    let estimate = || n.rt.block_on(n.client.estimate_fee()).unwrap();

    // No path-B entries yet: take this node's minimum token amount; the fee rate is this node's minimum packing fee
    // rate (0)
    let e = estimate();
    assert_eq!(e.path_b.amount, 1);
    assert_eq!(e.path_b.per_kib, 0);
    assert_eq!(e.feerate_sat_vb, 2, "fee rate of the funding wallet");

    // Three path-B ops (amounts 2, 9, 5, the same transfer), one per block: the median of the blocks' lowest fee rates
    // is the one with amount 5; converting an ordinary transfer from it gives 5 again
    for (nonce, amount) in [(0, 2), (1, 9), (2, 5)] {
        let op = signed(&alice, nonce, FeeMode::Token { amount }, bob.account, 1);
        n.wait_included(&n.submit(&op));
    }
    let e = estimate();
    assert_eq!(e.path_b.amount, 5);
    let op = signed(&alice, 3, FeeMode::Token { amount: 5 }, bob.account, 1);
    let cost = ksana_mempool::op_cost(
        &ksana_primitives::DEVNET,
        &ksana_primitives::DEVNET.rules(0),
        op.encode().unwrap().len(),
        op.max_gas,
    );
    assert_eq!(e.path_b.per_kib, ksana_mempool::fee_rate(5, cost));
    assert_eq!(e.at.height, n.block(e.at.height).header.height);

    // The settlement fee rate follows bitcoind's estimate, and is the floor while its mempool has room
    n.mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    assert_eq!(estimate().feerate_sat_vb, 10, "the estimate");
    n.mock.set_mempool_info(Some(MempoolInfo {
        vsize: 314_541,
        min_feerate: 1,
    }));
    assert_eq!(estimate().feerate_sat_vb, 2, "the floor");
}
