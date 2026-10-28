//! M11: the indexer crosses an upgrade's activation point (D-076). With the same upgrade table as the node it verifies
//! the state root block by block; an indexer without the upgrade (which does not know the new version) stops before the
//! fork, without judging a block invalid or halting.

mod common;

use common::*;
use ksana_primitives::{Params, Upgrade};

/// Protocol version 3 from DAA score 38: the chain below has one non-empty block (weight 1) plus two empty blocks per
/// round (streaks 1, 2, weights 12 and 24, D-084, D-090); block 3 has DAA score 37 and block 4 has 38, so version 3
/// starts at block 4.
const UPGRADES: &[Upgrade] = &[Upgrade::new(3, 38)];

#[test]
fn the_indexer_follows_an_upgrade_and_an_outdated_one_stops_before_it() {
    let params = Params {
        upgrades: UPGRADES,
        ..PARAMS
    };
    let (alice, bob) = (user("m11/indexer/alice"), user("m11/indexer/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::with_params(&mock, &genesis, "m11", params.clone());
    // Each round: one path-B op (with a settlement transaction) and two empty blocks, then the settlement lands on BTC
    for nonce in 0..4 {
        a.submit(transfer(&alice, nonce, token(2), bob.account, 1));
        assert!(a.mine().settlement.is_some());
        a.mine_empty();
        a.mine_empty();
        a.broadcast_settlements();
        mock.mine(1);
    }
    assert_eq!(a.block(3).header.version, 2);
    assert_eq!(a.block(4).header.version, 3);
    assert_eq!(a.block(a.tip_height()).header.version, 3);

    // Same upgrade table: crosses the activation point, matches the node block by block
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer_with(dir.path(), &genesis, &mock, params);
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    assert!(ix.status().height >= 5, "{:?}", ix.status());

    // An indexer without this upgrade: stops before the activation point, without judging a block invalid
    let old_dir = tempfile::tempdir().unwrap();
    let old = open_indexer_with(old_dir.path(), &genesis, &mock, PARAMS);
    catch_up(&old);
    let status = old.status();
    assert!(status.height < 4, "{status:?}");
    assert_eq!(status.halted, None);
    for height in 0..=status.height {
        assert_eq!(old.block_at(height), ix.block_at(height));
    }
}
