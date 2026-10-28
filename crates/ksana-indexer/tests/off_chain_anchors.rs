//! Blocks anchored off the BTC best chain under an honest chain (D-101 and its review): the finality determination
//! excludes a block whose own anchor, or the anchor of a block it merges, names another BTC block than the best
//! chain's at that height, together with every block whose selected chain passes through it. A node takes such blocks
//! from peers (D-056), so the honest chain must never merge one or build on one: otherwise a single fast-lane block
//! stops the finality of every node and indexer for good. Each attack runs next to its control, the same steps with the
//! attacker's block anchored on the real BTC block; finality must go on past the attacker's block in both.
//!
//! - merged: an empty block Y on an old main-chain block, anchored at that block's anchor height with a hash no BTC block
//!   has, handed to an honest node as a sync response (the anchor lags the BTC tip by more than MAX_ANCHOR_LAG +
//!   ANCHOR_DEPTH, so `check_anchor` does not look at the hash); the honest producer's next block must not merge it.
//! - two blocks: Y as above, and the attacker's empty block A on the honest tip merging Y, with its own anchor the real
//!   BTC tip, gossiped; A must not become the main-chain tip.
//! - on the tip: while the fast lane is idle the tip's own anchor comes to lag; the attacker's empty block A on the tip,
//!   anchored at the tip's anchor height with a fabricated hash, handed over as a sync response, must not become the tip.
//! - stale: the same on the tip with A anchored on a BTC block a 1-block BTC reorg just orphaned (a stale block bitcoind
//!   knows, taken from a peer whatever the lag).

mod common;

use std::sync::Arc;

use common::*;
use ksana_btc::client::BtcClient;
use ksana_btc::headers::{HeaderDag, Insert};
use ksana_btc::mock::MockBtc;
use ksana_btc::tracker::BtcSnapshot;
use ksana_consensus::{Consensus, PowConsensus};
use ksana_execution::GenesisFile;
use ksana_primitives::{Block, BlockHeader, BtcAnchor, Entry, BLOCK_VERSION};

fn daa_score_of(a: &Side, hash: &[u8; 32]) -> u64 {
    a.chain.ghostdag(hash).unwrap().unwrap().daa_score
}

/// An empty block on the node's block `parent` anchored at `anchor`, valid PoW, landed nowhere.
fn forge_empty_on(a: &Side, parent: &Block, anchor: BtcAnchor, seed: &str) -> Block {
    let parent_hash = parent.header.hash();
    let entries: Vec<Entry> = Vec::new();
    let mut header = BlockHeader {
        version: BLOCK_VERSION,
        height: parent.header.height + 1,
        parents: vec![parent_hash],
        btc_anchor: anchor,
        entries_root: ksana_crypto::entries_root(&entries).unwrap(),
        state_root: parent.header.state_root,
        producer: user(seed).account,
        timestamp: parent.header.timestamp + 1,
        // the same for an empty and a non-empty block with the same parents
        bits: a
            .chain
            .consensus()
            .next_bits(&parent.header, daa_score_of(a, &parent_hash) + 1, 1)
            .unwrap(),
        nonce: 0,
    };
    let streak = a.chain.streak_of(&parent_hash).unwrap().unwrap();
    let weight = a.chain.consensus().weight(&header, streak);
    assert!(a
        .chain
        .consensus()
        .solve(&mut header, weight, 0, 1 << 24)
        .unwrap());
    Block {
        header,
        entries,
        settlement: None,
    }
}

/// Returns (node final height, fresh indexer final height, height of M, whether M merged Y, node tip height).
fn run_merged(fabricated: bool) -> (u64, u64, u64, bool, u64) {
    let tag = if fabricated { "mu1" } else { "mu0" };
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, tag);
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    // BTC goes on: block 1's anchor now lags the tip by more than MAX_ANCHOR_LAG + ANCHOR_DEPTH
    mock.mine(3);
    a.sync();
    let block1 = a.block(1);
    let height = block1.header.btc_anchor.height;
    let real = a.chain.btc().snapshot().unwrap().hash_at(height).unwrap();
    let anchor = BtcAnchor {
        height,
        hash: if fabricated { [0xab; 32] } else { real },
    };
    let y = forge_empty_on(&a, &block1, anchor, &format!("{tag}/attacker"));
    let y_hash = y.header.hash();
    // A peer's sync response: a historical block (`now` is None)
    let accepted = a
        .chain
        .accept_block(&y, None)
        .map(|report| format!("{:?}", report.outcome));
    eprintln!("[off-chain merged] fabricated={fabricated}: Y {} (on block 1, anchor height {height}) accepted: {accepted:?}", hex::encode(y_hash));
    // The honest producer's next block
    a.submit(transfer(&alice, 1, token(2), bob.account, 1));
    let m = a.mine();
    let merged = m.header.parents.contains(&y_hash);
    eprintln!(
        "[off-chain merged] fabricated={fabricated}: honest block M at height {} has parents {:?}; merges Y: {merged}",
        m.header.height,
        m.header.parents.iter().map(hex::encode).collect::<Vec<_>>()
    );
    // The honest chain goes on and lands, then gets confirmations
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    for nonce in 2..5 {
        a.submit(transfer(&alice, nonce, token(2), bob.account, 1));
        a.mine();
        a.mine_empty();
        a.mine_empty();
        a.broadcast_settlements();
        mock.mine(1);
        a.sync();
    }
    mock.mine(PARAMS.btc_finality_confs + 2);
    a.sync();
    a.sync();
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    let status = ix.status();
    let result = (
        a.chain.finalized().unwrap().1,
        status.finalized_height,
        m.header.height,
        merged,
        a.tip_height(),
    );
    eprintln!(
        "[off-chain merged] fabricated={fabricated}: (node final, indexer final, M height, M merges Y, node tip) = {result:?}; settled {}; indexer halted = {:?}",
        a.settled_height(),
        status.halted
    );
    result
}

#[test]
fn an_honest_block_never_merges_a_synced_block_anchored_off_the_btc_chain() {
    let control = run_merged(false);
    let attack = run_merged(true);
    // Control: M merges Y, and finality passes M
    assert!(control.3, "control: {control:?}");
    assert!(
        control.0 > control.2 && control.1 > control.2,
        "control: {control:?}"
    );
    // Attack (on e3d230d M merges Y, and neither the node nor the indexer gets past the block before M)
    assert!(!attack.3, "M merges Y: {attack:?}");
    assert!(
        attack.0 > attack.2 && attack.1 > attack.2,
        "attack: {attack:?}"
    );
}

/// Forges an empty header on `parents` (the first is the selected parent) in `dag`, as the nodes compute weight, GHOSTDAG
/// data and `bits`; valid PoW; added to `dag`. (The same as `forge_in` in `indexer.rs`.)
fn forge_in(
    dag: &mut HeaderDag,
    consensus: &PowConsensus,
    parents: Vec<[u8; 32]>,
    btc_anchor: BtcAnchor,
    state_root: [u8; 32],
    seed: &str,
) -> BlockHeader {
    let sp = dag.header(&parents[0]).unwrap().clone();
    let timestamp = parents
        .iter()
        .map(|p| dag.header(p).unwrap().timestamp)
        .max()
        .unwrap()
        + 1;
    let mut header = BlockHeader {
        version: BLOCK_VERSION,
        height: sp.height + 1,
        parents,
        btc_anchor,
        entries_root: ksana_crypto::entries_root(&[]).unwrap(),
        state_root,
        producer: user(seed).account,
        timestamp,
        bits: 0,
        nonce: 0,
    };
    let sp_streak = dag.dag().parent_streak(&header.parents).unwrap();
    let weight = consensus.weight(&header, sp_streak);
    let data = dag.dag().compute(&header.parents, weight).unwrap();
    assert_eq!(data.selected_parent, Some(sp.hash()));
    header.bits = consensus.next_bits(&sp, data.daa_score, weight).unwrap();
    assert!(consensus.solve(&mut header, weight, 0, 1 << 24).unwrap());
    assert_eq!(
        dag.insert(consensus, header.clone(), &BtcSnapshot::default()),
        Insert::Connected(1)
    );
    header
}

/// The two-block variant: Y as above, and the attacker's empty block A on the honest tip (block 3) merging Y, with its
/// own anchor the real BTC tip, gossiped. Returns (node final, fresh indexer final, A's height, whether A became the
/// node's main-chain tip, whether the next honest block builds on A, node tip height).
fn run_two_blocks(fabricated: bool) -> (u64, u64, u64, bool, bool, u64) {
    let tag = if fabricated { "mv1" } else { "mv0" };
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, tag);
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    mock.mine(3);
    a.sync();
    let block1 = a.block(1);
    let block3 = a.block(3);
    let height = block1.header.btc_anchor.height;
    let real = a.chain.btc().snapshot().unwrap().hash_at(height).unwrap();
    let y = forge_empty_on(
        &a,
        &block1,
        BtcAnchor {
            height,
            hash: if fabricated { [0xab; 32] } else { real },
        },
        &format!("{tag}/attacker"),
    );
    let consensus = a.chain.consensus();
    let mut dag = HeaderDag::new(&PARAMS, consensus, a.chain.genesis().clone()).unwrap();
    for header in (1..=3).map(|h| a.block(h).header).chain([y.header.clone()]) {
        assert_eq!(
            dag.insert(consensus, header, &BtcSnapshot::default()),
            Insert::Connected(1)
        );
    }
    let (tip_height, tip_hash) = mock.tip().unwrap();
    let attacker = forge_in(
        &mut dag,
        consensus,
        vec![block3.header.hash(), y.header.hash()],
        BtcAnchor {
            height: tip_height,
            hash: tip_hash,
        },
        block3.header.state_root,
        &format!("{tag}/attacker"),
    );
    let attacker = Block {
        header: attacker,
        entries: Vec::new(),
        settlement: None,
    };
    let y_result = a
        .chain
        .accept_block(&y, None)
        .map(|r| format!("{:?}", r.outcome));
    let now = attacker.header.timestamp;
    let a_result = a
        .chain
        .accept_block(&attacker, Some(now))
        .map(|r| format!("{:?}", r.outcome));
    let attacker_hash = attacker.header.hash();
    let on_top = a.chain.tip().unwrap().hash == attacker_hash;
    eprintln!("[off-chain two blocks] fabricated={fabricated}: Y by sync: {y_result:?}; A by gossip: {a_result:?}; A is the main-chain tip: {on_top}");
    a.submit(transfer(&alice, 1, token(2), bob.account, 1));
    let m = a.mine();
    let builds_on = m.header.parents.first() == Some(&attacker_hash);
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    for nonce in 2..5 {
        a.submit(transfer(&alice, nonce, token(2), bob.account, 1));
        a.mine();
        a.mine_empty();
        a.mine_empty();
        a.broadcast_settlements();
        mock.mine(1);
        a.sync();
    }
    mock.mine(PARAMS.btc_finality_confs + 2);
    a.sync();
    a.sync();
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    let status = ix.status();
    let result = (
        a.chain.finalized().unwrap().1,
        status.finalized_height,
        attacker.header.height,
        on_top,
        builds_on,
        a.tip_height(),
    );
    eprintln!(
        "[off-chain two blocks] fabricated={fabricated}: (node final, indexer final, A height, A is tip, M builds on A, node tip) = {result:?}; settled {}",
        a.settled_height()
    );
    result
}

#[test]
fn an_attacker_block_on_the_tip_merging_a_block_anchored_off_the_btc_chain_does_not_stop_finality()
{
    let control = run_two_blocks(false);
    let attack = run_two_blocks(true);
    assert!(control.3 && control.4, "control: {control:?}");
    assert!(
        control.0 > control.2 && control.1 > control.2,
        "control: {control:?}"
    );
    // Attack (on e3d230d A becomes the main-chain tip, the honest chain builds on it and finality stops below A)
    assert!(
        !attack.3 && !attack.4,
        "the honest chain builds on A: {attack:?}"
    );
    assert!(
        attack.0 > attack.2 && attack.1 > attack.2,
        "attack: {attack:?}"
    );
}

/// The honest chain goes on after the attack (path B transfers with nonces 1–4, each block landed on BTC), then BTC
/// confirms it; returns (node final height, fresh indexer final height, node tip height).
fn go_on_and_measure(
    a: &Side,
    mock: &Arc<MockBtc>,
    genesis: &GenesisFile,
    alice: &User,
    bob: &User,
) -> (u64, u64, u64) {
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    for nonce in 2..5 {
        a.submit(transfer(alice, nonce, token(2), bob.account, 1));
        a.mine();
        a.mine_empty();
        a.mine_empty();
        a.broadcast_settlements();
        mock.mine(1);
        a.sync();
    }
    mock.mine(PARAMS.btc_finality_confs + 2);
    a.sync();
    a.sync();
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), genesis, mock);
    catch_up(&ix);
    let status = ix.status();
    (
        a.chain.finalized().unwrap().1,
        status.finalized_height,
        a.tip_height(),
    )
}

/// The one-block variant on the tip: while the fast lane is idle, BTC moves on and the main-chain tip's own anchor comes
/// to lag the BTC tip by more than MAX_ANCHOR_LAG + ANCHOR_DEPTH; the attacker's empty block A on the tip, anchored at
/// the tip's anchor height with a hash no BTC block has, is handed over as a sync response. With `stale`, the anchor is
/// instead a BTC block a 1-block BTC reorg just orphaned (a stale block bitcoind knows), and the fast-lane tip's anchor
/// does not lag. Returns (node final, fresh indexer final, A's height, whether A became the main-chain tip, whether
/// the next honest block builds on A, node tip height).
fn run_on_tip(fabricated: bool, stale: bool) -> (u64, u64, u64, bool, bool, u64) {
    let tag = match (fabricated, stale) {
        (false, false) => "md0",
        (true, false) => "md1",
        (false, true) => "ms0",
        (true, true) => "ms1",
    };
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, tag);
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    a.sync();
    let block3 = a.block(3);
    let anchor = if stale {
        // A natural 1-block BTC reorg at the tip: the orphaned block stays known to bitcoind (`in_best_chain` false)
        let (height, orphaned) = mock.tip().unwrap();
        assert_eq!(mock.reorg(1), vec![orphaned]);
        mock.mine(1);
        a.sync();
        let (_, replacement) = mock.tip().unwrap();
        BtcAnchor {
            height,
            hash: if fabricated { orphaned } else { replacement },
        }
    } else {
        // The fast lane is idle while BTC goes on: block 3's anchor now lags the BTC tip by more than MAX_ANCHOR_LAG +
        // ANCHOR_DEPTH
        mock.mine(3);
        a.sync();
        let height = block3.header.btc_anchor.height;
        let real = a.chain.btc().snapshot().unwrap().hash_at(height).unwrap();
        BtcAnchor {
            height,
            hash: if fabricated { [0xab; 32] } else { real },
        }
    };
    let attacker = forge_empty_on(&a, &block3, anchor, &format!("{tag}/attacker"));
    let attacker_hash = attacker.header.hash();
    // A peer's sync response (`now` is None)
    let accepted = a
        .chain
        .accept_block(&attacker, None)
        .map(|r| format!("{:?}", r.outcome));
    let on_top = a.chain.tip().unwrap().hash == attacker_hash;
    eprintln!(
        "[off-chain {tag}] A {} on block 3, anchor height {}: {accepted:?}; A is the main-chain tip: {on_top}",
        hex::encode(attacker_hash),
        attacker.header.btc_anchor.height
    );
    a.submit(transfer(&alice, 1, token(2), bob.account, 1));
    let m = a.mine();
    let builds_on = m.header.parents.first() == Some(&attacker_hash);
    let (node_final, ix_final, tip) = go_on_and_measure(&a, &mock, &genesis, &alice, &bob);
    let result = (
        node_final,
        ix_final,
        attacker.header.height,
        on_top,
        builds_on,
        tip,
    );
    eprintln!(
        "[off-chain {tag}] (node final, indexer final, A height, A is tip, next honest block builds on A, node tip) = {result:?}; settled {}",
        a.settled_height()
    );
    result
}

#[test]
fn an_attacker_block_on_an_idle_tip_with_a_fabricated_anchor_does_not_stop_finality() {
    let control = run_on_tip(false, false);
    let attack = run_on_tip(true, false);
    assert!(control.3 && control.4, "control: {control:?}");
    assert!(
        control.0 > control.2 && control.1 > control.2,
        "control: {control:?}"
    );
    // Attack (on e3d230d A becomes the main-chain tip, the honest chain builds on it and finality stops below A)
    assert!(
        !attack.3 && !attack.4,
        "the honest chain builds on A: {attack:?}"
    );
    assert!(
        attack.0 > attack.2 && attack.1 > attack.2,
        "attack: {attack:?}"
    );
}

#[test]
fn an_attacker_block_on_the_tip_anchored_on_a_stale_btc_block_does_not_stop_finality() {
    let control = run_on_tip(false, true);
    let attack = run_on_tip(true, true);
    assert!(control.3 && control.4, "control: {control:?}");
    assert!(
        control.0 > control.2 && control.1 > control.2,
        "control: {control:?}"
    );
    // Attack (on e3d230d A becomes the main-chain tip, the honest chain builds on it and finality stops below A)
    assert!(
        !attack.3 && !attack.4,
        "the honest chain builds on A: {attack:?}"
    );
    assert!(
        attack.0 > attack.2 && attack.1 > attack.2,
        "attack: {attack:?}"
    );
}
