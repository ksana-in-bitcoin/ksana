//! Node behavior of M6.2 (SPEC §16) on the in-memory mock BTC chain: the settlement funding wallet and the revenue
//! wallet are separate (D-060).

mod common;

use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::{ScriptBuf, WPubkeyHash};
use common::mock_node::{signed, MockNode};
use common::user;
use ksana_node::producer::Revenue;
use ksana_primitives::{FeeMode, Params, DEVNET};
use ksana_rpc::SettlementState;

/// Hot wallet 8 × 500 000 sats, cap 2 400 000 sats. Each settlement transaction uses one funding input (the change
/// is output 0): for the first 3, the hot wallet still holds 3.5 / 3.0 / 2.5 million sats after removing the funding,
/// not below the cap, so the change goes to the revenue address; after that the remainder is below the cap and the
/// change returns to the hot wallet. Besides the blocks of the 5 path-B ops, blocks with unlock entries may also carry
/// their own settlement transaction, likewise with one funding input each, so we only check that the change of all
/// settlement transactions on the main chain is the revenue address for the first 3 and the hot wallet for all the
/// rest. The block reward is removed (D-074) to reduce unlock entries.
///
/// Settlement batching is off (D-077): each round below waits for the settlement transaction of the block holding this
/// round's op to be broadcast before mining a BTC block. With batching on, if a block holding only an unlock entry
/// carries the settlement transaction first, the body of the block holding the op has to wait for it to confirm on BTC
/// and be carried by a later block, and the two sides wait for each other (in CI run 59 the unlock entry and the op of
/// round 3 landed in two blocks and the wait timed out).
#[test]
fn settlement_change_goes_to_the_revenue_address_while_the_hot_wallet_stays_above_the_cap() {
    let (alice, bob) = (user("m6.2/alice"), user("m6.2/bob"));
    let revenue = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([7; 20]));
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
                    hot_cap_sat: 2_400_000,
                })
                .with_batching(false)
        },
    );
    for nonce in 0..5 {
        let op = signed(&alice, nonce, FeeMode::Token { amount: 2 }, bob.account, 1);
        let height = n.wait_included(&n.submit(&op)).block.unwrap().height;
        n.wait_settlement(height, SettlementState::Broadcast);
        n.mock.mine(1);
        n.wait_settlement(height, SettlementState::Confirmed);
    }
    // The settlement transactions carried by the blocks on the main chain, by height
    let mut changes = Vec::new();
    for height in 1..=n.tip_height() {
        let Some(txs) = n.block(height).settlement else {
            continue;
        };
        // The change is in the commit transaction (D-081)
        let pair = ksana_btc::settlement::SettlementPair::decode(&txs).unwrap();
        assert_eq!(pair.commit.input.len(), 1, "one funding input");
        changes.push(pair.commit.output[0].script_pubkey.clone());
    }
    let hot = n.funding.script.clone();
    assert!(changes.len() >= 5, "{changes:?}");
    assert_eq!(changes[..3], [revenue.clone(), revenue.clone(), revenue]);
    assert!(changes[3..].iter().all(|s| *s == hot), "{changes:?}");
}
