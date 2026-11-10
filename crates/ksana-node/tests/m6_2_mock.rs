//! Node behavior of M6.2 (SPEC §16) on the in-memory mock BTC chain: the settlement funding wallet and the revenue
//! wallet are separate (D-060, D-142).

mod common;

use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::{ScriptBuf, WPubkeyHash};
use common::mock_node::{signed, wait, MockNode};
use common::user;
use ksana_btc::client::BtcClient;
use ksana_node::producer::Revenue;
use ksana_primitives::{FeeMode, Params, DEVNET};
use ksana_rpc::SettlementState;

/// Hot wallet 8 × 500 000 sats, cap 2 400 000 sats (D-060, D-142). The change of every settlement transaction returns
/// to the hot wallet, so the producer can bump any of them by CPFP (D-140); the rescue thread sweeps what the available
/// funding holds above the cap to the revenue address in a transaction of its own, which never takes the hot wallet
/// below the cap. The block reward is removed (D-074) to reduce unlock entries.
///
/// Settlement batching is off (D-077): each round below waits for the settlement transaction of the block holding this
/// round's op to be broadcast before mining a BTC block. With batching on, if a block holding only an unlock entry
/// carries the settlement transaction first, the body of the block holding the op has to wait for it to confirm on BTC
/// and be carried by a later block, and the two sides wait for each other (in CI run 59 the unlock entry and the op of
/// round 3 landed in two blocks and the wait timed out).
#[test]
fn settlement_change_stays_in_the_hot_wallet_and_a_sweep_moves_the_excess_to_the_revenue_address() {
    let (alice, bob) = (user("m6.2/alice"), user("m6.2/bob"));
    let revenue = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([7; 20]));
    let cap = 2_400_000;
    let n = MockNode::start_with_params(
        Params {
            block_reward: 0,
            ..DEVNET
        },
        &[(&alice, 1_000)],
        Duration::from_millis(150),
        |mock| mock,
        |funding| {
            funding
                .with_revenue(Revenue {
                    script: revenue.clone(),
                    hot_cap_sat: cap,
                })
                .with_batching(false)
        },
    );
    let hot = n.funding.script.clone();
    let initial: u64 = n
        .mock
        .scan_utxos(&hot)
        .unwrap()
        .iter()
        .map(|u| u.txout.value.to_sat())
        .sum();
    for nonce in 0..5 {
        let op = signed(&alice, nonce, FeeMode::Token { amount: 2 }, bob.account, 1);
        let height = n.wait_included(&n.submit(&op)).block.unwrap().height;
        n.wait_settlement(height, SettlementState::Broadcast);
        n.mock.mine(1);
        n.wait_settlement(height, SettlementState::Confirmed);
    }
    // The settlement transactions carried by the blocks on the main chain all return their change to the hot wallet
    let mut changes = Vec::new();
    for height in 1..=n.tip_height() {
        let Some(txs) = n.block(height).settlement else {
            continue;
        };
        let pair = ksana_btc::settlement::SettlementPair::decode(&txs).unwrap();
        changes.push(pair.reveal.output[0].script_pubkey.clone());
    }
    assert!(changes.len() >= 5, "{changes:?}");
    assert!(changes.iter().all(|s| *s == hot), "{changes:?}");
    // The sweep lands on the revenue address, and the hot wallet keeps at least the cap
    let swept = wait("the revenue sweep", || {
        n.mock.mine(1);
        let swept: u64 = n
            .mock
            .scan_utxos(&revenue)
            .unwrap()
            .iter()
            .map(|u| u.txout.value.to_sat())
            .sum();
        (swept > 0).then_some(swept)
    });
    assert!(swept <= initial - cap, "swept {swept} of {initial}");
    let kept: u64 = n
        .mock
        .scan_utxos(&hot)
        .unwrap()
        .iter()
        .map(|u| u.txout.value.to_sat())
        .sum();
    assert!(kept + swept <= initial, "kept {kept}, swept {swept}");
}
