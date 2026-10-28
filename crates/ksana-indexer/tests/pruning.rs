//! M25 pruning in the indexer (D-097): an in-process node settles every block on the mock BTC chain, blocks become
//! BTC-final one after another, and the indexer prunes.
//!
//! - Memory holds only the pruning point and its future; the index answers for every executed height, and the block
//!   hash and `state_root` at every height still equal the node's.
//! - A restart continues from the pruning checkpoint (the pruning root is known before the first sync, the executed
//!   main chain below it comes from the index) and goes on matching the node.
//! - An unusable checkpoint makes the indexer scan BTC from the genesis anchor again and verify its state store; a
//!   lost index makes it start over from the genesis anchor (the data kept since pruning no longer reaches back). Both
//!   converge on the node's chain.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::*;
use ksana_btc::mock::MockBtc;
use ksana_btc::prune::Pruning;
use ksana_execution::GenesisFile;
use ksana_indexer::{Indexer, IndexerConfig, Progress};
use ksana_primitives::Params;

/// A small DAG (as in the node's M25 tests): K = 2 and MERGE_DEPTH = 4 make `min_pruning_depth` 19; data is final
/// after 2 BTC confirmations.
fn small_dag() -> Params {
    let mut params = PARAMS;
    params.ghostdag_k = 2;
    params.merge_depth = 4;
    params.btc_finality_confs = 2;
    params
}

const MIN_PRUNING_DEPTH: u64 = 19;

/// Pruning with a margin of 2 BTC blocks, whenever the pruning point can advance by 5 blue score.
fn open_pruned(dir: &Path, genesis: &GenesisFile, mock: &Arc<MockBtc>) -> Indexer {
    Indexer::open(IndexerConfig {
        data_dir: dir.to_path_buf(),
        genesis: genesis.clone(),
        params: small_dag(),
        btc: mock.clone(),
        pow_algorithm: None,
        pruning: Pruning {
            enabled: true,
            btc_margin: 2,
            step: 5,
        },
    })
    .unwrap()
}

/// The node produces a block carrying a transfer of 1 KSA from `from` (nonce `nonce`) to `to`, broadcasts the
/// settlements that are due and mines one BTC block; the indexer catches up.
fn step(side: &Side, ix: &Indexer, from: &User, nonce: u64, to: [u8; 32]) -> Progress {
    side.submit(transfer(from, nonce, token(3), to, 1));
    side.mine();
    side.broadcast_settlements();
    side.mock.mine(1);
    catch_up(ix)
}

#[test]
fn the_pruned_indexer_matches_the_node_and_restarts_from_the_checkpoint() {
    let (alice, bob) = (user("m25/alice"), user("m25/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let side = Side::with_params(&mock, &genesis, "m25", small_dag());
    let dir = tempfile::tempdir().unwrap();
    let ix = open_pruned(dir.path(), &genesis, &mock);
    let mut progress = None;
    for nonce in 0..45 {
        progress = Some(step(&side, &ix, &alice, nonce, bob.account));
    }
    let progress = progress.unwrap();
    let (root, root_height) = ix.pruning_root().expect("the indexer has pruned");
    let point = ix.pruning_point().unwrap();
    let point_height = side.chain.accepted_height(&point).unwrap().unwrap();
    assert_eq!(side.block(root_height).header.hash(), root);
    // A single chain: blue score is height
    assert_eq!(root_height - point_height, MIN_PRUNING_DEPTH);
    assert_eq!(
        ix.dag_len() as u64,
        progress.main_height - point_height + 1,
        "the DAG holds the pruning point and its future only"
    );
    assert!(dir.path().join("pruning.ckpt").exists());
    assert_matches_node(&ix, &side);
    // Blocks executed below the pruning point answer from the index
    let first = side.block(1);
    assert_eq!(
        ix.find_block(&first.header.hash()),
        Some((1, first.header.state_root))
    );

    // Restart: continues from the checkpoint
    let pruned = ix.pruning_root();
    let height = ix.status().height;
    drop(ix);
    let ix = open_pruned(dir.path(), &genesis, &mock);
    assert_eq!(ix.pruning_root(), pruned);
    assert_eq!(ix.pruning_point(), Some(point));
    assert_eq!(ix.status().height, height);
    assert_eq!(
        ix.block_at(1),
        None,
        "not before verification against the BTC data"
    );
    catch_up(&ix);
    assert_matches_node(&ix, &side);

    // Goes on, and prunes again
    for nonce in 45..55 {
        step(&side, &ix, &alice, nonce, bob.account);
    }
    let (_, later) = ix.pruning_root().unwrap();
    assert!(later > root_height, "the pruning root advanced");
    assert_matches_node(&ix, &side);
}

#[test]
fn a_bad_checkpoint_or_a_lost_index_starts_over_and_converges() {
    let (alice, bob) = (user("m25/ckpt/alice"), user("m25/ckpt/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let side = Side::with_params(&mock, &genesis, "m25ckpt", small_dag());
    let dir = tempfile::tempdir().unwrap();
    let ix = open_pruned(dir.path(), &genesis, &mock);
    for nonce in 0..40 {
        step(&side, &ix, &alice, nonce, bob.account);
    }
    assert!(ix.pruning_root().is_some());
    let height = ix.status().height;
    drop(ix);

    // A corrupt checkpoint is not used: BTC is scanned from the genesis anchor again, the state store verified
    let path = dir.path().join("pruning.ckpt");
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    let ix = open_pruned(dir.path(), &genesis, &mock);
    assert_eq!(ix.pruning_root(), None);
    catch_up(&ix);
    assert_eq!(ix.status().height, height);
    assert_matches_node(&ix, &side);
    assert!(ix.pruning_root().is_some(), "pruned again");
    drop(ix);

    // The index is lost: the executed main chain below the pruning root cannot be rebuilt, so the indexer starts over
    // from the genesis anchor
    std::fs::remove_dir_all(dir.path().join("index")).unwrap();
    let ix = open_pruned(dir.path(), &genesis, &mock);
    assert!(ix.pruning_root().is_some());
    catch_up(&ix);
    assert_matches_node(&ix, &side);
    assert!(ix.pruning_root().is_some(), "pruned again");
    for nonce in 40..45 {
        step(&side, &ix, &alice, nonce, bob.account);
    }
    assert_matches_node(&ix, &side);
}

/// Second audit, P5-2: an indexer catching up through a long history (here a fresh one, over several tracker rounds of
/// 500 BTC blocks) prunes after every round, as the node does after every BTC sync, instead of holding the whole history
/// until it reaches the BTC tip: one call of `sync_once` prunes several times, and the result matches the node.
#[test]
fn a_fresh_indexer_prunes_while_it_catches_up() {
    let (alice, bob) = (user("m25/fresh/alice"), user("m25/fresh/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let side = Side::with_params(&mock, &genesis, "m25fresh", small_dag());
    // About 20 blocks per 500 BTC blocks, over four tracker rounds and a bit
    for nonce in 0..85 {
        side.submit(transfer(&alice, nonce, token(3), bob.account, 1));
        side.mine();
        side.broadcast_settlements();
        side.mock.mine(25);
    }
    assert!(mock.tip_height() > 2_000);
    let dir = tempfile::tempdir().unwrap();
    let ix = open_pruned(dir.path(), &genesis, &mock);
    let progress = ix.sync_once().unwrap();
    assert_eq!(progress.btc_height, Some(mock.tip_height()));
    assert!(
        progress.prunes >= 3,
        "pruned {} times while catching up",
        progress.prunes
    );
    catch_up(&ix);
    assert_matches_node(&ix, &side);
    assert!(dir.path().join("pruning.ckpt").exists());
}
