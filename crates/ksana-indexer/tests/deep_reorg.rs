//! The BTC reorg depth sweep (D-104). For an anchor depth d and a BTC reorg of k blocks above a fork point F, a node
//! producing blocks, an indexer following it and a node syncing from the genesis block afterwards end on the same main
//! chain with the same state roots:
//!
//! - k ≤ d: the fast lane does not change (every block's anchor is at or below F);
//! - k ≥ d + 1: the blocks anchored above F, and the blocks on top of them, leave the main chain: the node retreats to
//!   just below the lowest of them, and with k ≤ `BTC_FINALITY_CONFS` no block that was final leaves the main chain
//!   (no new branch here carries a heavier chain: one that does takes back what was decided at the BTC blocks it
//!   replaces, see [`a_reorg_of_the_btc_block_that_decided_a_block_with_a_heavier_chain`], D-114);
//! - afterwards no main-chain block above the finalized tip is anchored off the BTC best chain, finality goes past the
//!   retreat once the new chain's data lands, a path B op is included exactly once, and a path C op is included again
//!   exactly when its native transaction is on the new branch.
//!
//! The contents on the orphaned BTC blocks ([`Content`]): empty blocks only, a path B op, a path C op whose native
//! transaction confirms again on the new branch or is double-spent there, settlement transactions waiting in the BTC
//! mempool (they keep their funding inputs, and the chain they land later does not set off republishing) or landed on
//! the old branch, and BTC going back to the old branch. `cargo test` runs the boundaries (k = d and d + 1 for every d,
//! every content once, and k = 7, which removes the data of a final block) and twelve random cases with k up to 8,
//! extra fast-lane blocks on the old branch and a BTC sync between the reorg and the new branch ([`Timing`]); the full
//! sweep is ignored by default:
//!
//! ```sh
//! cargo test -p ksana-indexer --release --test deep_reorg -- --ignored --nocapture
//! ```
//!
//! More cases: a retreat that returns more ops than the entry pool holds, a producer that sees the reorg late and
//! builds on a block it orphaned, a block a retreat left behind, which `Status` does not carry while BTC stays off its
//! branch, and the blocks built on it, which a node refusing it keeps as no orphans, a path C op that, included again
//! in the new BTC order, fails out of turn, and how deep a reorg must be for that with mainnet's depths:
//! `ANCHOR_DEPTH` + 2 blocks along one chain of blocks, and when the path B op below it is in a parallel block,
//! `ANCHOR_DEPTH` + 2 as the next block merges it again (D-104 item k, local policy) and `ANCHOR_DEPTH` + 1 when it
//! does not. Then the nonces the entry pool counts as taken by path C ops waiting for forced inclusion: the path B ops
//! a retreat returns above one, only ops a block forcing them would authenticate, in BTC order, and only from the
//! heights the next blocks force.

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::{
    absolute, transaction, Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use common::*;
use ksana_btc::client::BtcClient;
use ksana_btc::payload::funding_inputs;
use ksana_btc::settlement::SettlementPair;
use ksana_execution::{keys, OpStatus, StateSource};
use ksana_indexer::rpc::StatusJson;
use ksana_indexer::Indexer;
use ksana_mempool::{Mempool, Policy};
use ksana_node::chain::{Accepted, REMEMBERED};
use ksana_node::producer::FeePolicy;
use ksana_node::rpc::NodeRpc;
use ksana_node::NodeError;
use ksana_primitives::{Block, BtcAnchor, Entry, Params, VmOp, KSA_ASSET_ID};
use ksana_rpc::{Backend, OpState};
use proptest::prelude::*;

/// What the orphaned BTC blocks carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    /// Only empty fast-lane blocks anchor on them.
    Empty,
    /// A block with a path B op anchors on them.
    PathB,
    /// A block with a path C op anchors on them; its native transaction confirms again on the new branch.
    NativeAgain,
    /// Same, but another transaction spends the native transaction's input on the new branch.
    NativeSpent,
    /// A block with a path B op anchors on them; its settlement transactions wait in the BTC mempool through the reorg
    /// and land after the node's next settlement.
    SettlementWaiting,
    /// Same, but its settlement transactions landed on the old branch.
    SettlementLanded,
    /// Same as `SettlementLanded`, and BTC goes back to the old branch after the fast lane moved on.
    ReorgBack,
}

const CONTENTS: [Content; 7] = [
    Content::Empty,
    Content::PathB,
    Content::NativeAgain,
    Content::NativeSpent,
    Content::SettlementWaiting,
    Content::SettlementLanded,
    Content::ReorgBack,
];

impl Content {
    /// Whether the content block carries alice's path B op of nonce 1.
    fn path_b(self) -> bool {
        matches!(
            self,
            Content::PathB
                | Content::SettlementWaiting
                | Content::SettlementLanded
                | Content::ReorgBack
        )
    }

    /// Whether the content block's settlement transactions are broadcast before the reorg.
    fn broadcast(self) -> bool {
        matches!(
            self,
            Content::SettlementWaiting | Content::SettlementLanded | Content::ReorgBack
        )
    }

    /// Whether they land on the old branch, in the BTC block after the content block's.
    fn lands(self) -> bool {
        matches!(self, Content::SettlementLanded | Content::ReorgBack)
    }

    /// Whether the case exists for (d, k): landing on the old branch needs a BTC block above the content block's (k ≥
    /// d + 2).
    fn fits(self, d: u32, k: u32) -> bool {
        !self.lands() || k >= d + 2
    }
}

/// When things happen around the reorg.
#[derive(Clone, Copy, Debug, Default)]
struct Timing {
    /// Empty fast-lane blocks produced on each BTC block of the old branch after its own.
    extra: u32,
    /// Whether the node syncs BTC between the reorg taking the old branch off and the new branch being mined, seeing
    /// a shorter BTC chain on the way.
    midway: bool,
}

/// One case: anchor depth `d`, a BTC reorg of `k` blocks, `content` on the orphaned BTC blocks.
fn run(d: u32, k: u32, content: Content) {
    run_timed(d, k, content, Timing::default());
}

/// Like [`run`], with the given [`Timing`].
fn run_timed(d: u32, k: u32, content: Content, timing: Timing) {
    if !content.fits(d, k) {
        return;
    }
    let params = Params {
        anchor_depth: d,
        ..PARAMS
    };
    let confs = params.btc_finality_confs;
    let tag = format!(
        "deep-reorg/{d}/{k}/{content:?}/{}{}",
        timing.extra,
        if timing.midway { "/midway" } else { "" }
    );
    let name = |who: &str| user(&format!("{tag}/{who}"));
    let (alice, bob, carol) = (name("alice"), name("bob"), name("carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::with_params(&mock, &genesis, &tag, params.clone());
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer_with(dir.path(), &genesis, &mock, params.clone());

    // history below the fork point: a block with a body, landed and BTC-final
    mock.mine(d + 1);
    a.submit(transfer(&alice, 0, token(2), bob.account, 1));
    a.mine();
    land(&a);
    mock.mine(confs);
    a.sync();
    let (history_hash, history_height) = a.chain.finalized().unwrap();
    assert!(history_height >= 1, "{tag}");

    // the native transaction of a path C op is funded below the fork point and confirms on the old branch's first block
    let native = matches!(content, Content::NativeAgain | Content::NativeSpent).then(|| {
        native_tx(&mock, |input| {
            transfer_native(&carol, 0, input, bob.account, 4)
        })
    });
    let fork = mock.tip_height();
    catch_up(&ix);
    assert_matches_node(&ix, &a);

    // the old branch: k BTC blocks and a fast-lane block on each; the first fast-lane block anchored above the fork
    // point (on the old branch's (d + 1)th block) carries the content
    let content_step = if k > d { d + 1 } else { k };
    let path_b = transfer(&alice, 1, token(2), bob.account, 7);
    let mut above = Vec::new();
    let mut content_block = None;
    let mut old_pair = None;
    for step in 1..=k {
        match (&native, step) {
            (Some((tx, _)), 1) => {
                mock.mine_block(vec![tx.clone()]).unwrap();
            }
            _ => {
                mock.mine(1);
            }
        }
        a.sync();
        let mut produced = Vec::new();
        if step == content_step {
            if content.path_b() {
                a.submit(path_b.clone());
            }
            let block = a.mine();
            if content.path_b() {
                assert!(includes(&block, &path_b), "{tag}: {:?}", block.entries);
            }
            if let Some((_, op)) = &native {
                assert_eq!(includes(&block, op), k > d, "{tag}: {:?}", block.entries);
            }
            produced.push(block.clone());
            if content.broadcast() {
                produced.push(a.mine_empty());
                produced.push(a.mine_empty());
                a.broadcast_settlements();
                let pair = pair_of(&block);
                assert!(in_mempool(&mock, &pair.txid()), "{tag}");
                if content == Content::SettlementWaiting {
                    mock.hold(&pair.commit_txid());
                    mock.hold(&pair.txid());
                }
                old_pair = Some(pair);
            }
            content_block = Some(block);
        } else {
            produced.push(a.mine_empty());
        }
        for _ in 0..timing.extra {
            produced.push(a.mine_empty());
        }
        if step > d {
            above.extend(produced);
        }
    }
    let content_block = content_block.unwrap();
    if content.lands() {
        let snapshot = a.chain.btc().snapshot().unwrap();
        assert!(
            a.chain
                .carried_on_chain(&content_block, &snapshot)
                .unwrap()
                .is_some(),
            "{tag}"
        );
    }
    let tip_before = a.chain.tip().unwrap().hash;
    let final_before = a.chain.finalized().unwrap();
    // with no heavier chain on the new branch, only a reorg deeper than BTC_FINALITY_CONFS can take a final block
    // (D-114): the content block becomes final on the old branch when its data landed BTC_FINALITY_CONFS blocks before
    // the old tip
    if content.lands() && k >= d + 1 + confs {
        assert!(final_before.1 >= content_block.header.height, "{tag}");
    }
    catch_up(&ix);
    assert_matches_node(&ix, &a);

    // the reorg: k blocks off, a longer new branch (whose first block spends the native transaction's input elsewhere)
    let orphaned = mock.reorg(k);
    assert_eq!(orphaned.len(), k as usize, "{tag}");
    if timing.midway {
        // no block at the orphaned heights: an anchor above the BTC tip is not off the best chain
        a.sync();
        assert_eq!(a.chain.tip().unwrap().hash, tip_before, "{tag}");
    }
    match &native {
        Some((tx, _)) if content == Content::NativeSpent => {
            mock.mine_block(vec![rival(tx)]).unwrap();
            mock.mine(k);
        }
        _ => {
            mock.mine(k + 1);
        }
    }
    // the producer's anchor reaches the new branch's first block (where a native transaction confirms again)
    mock.mine(d.saturating_sub(k));
    a.sync();
    if k <= d {
        assert!(above.is_empty(), "{tag}");
        assert_eq!(
            a.chain.tip().unwrap().hash,
            tip_before,
            "{tag}: a reorg of at most d blocks changes nothing"
        );
    } else {
        let lowest = &above[0];
        assert_eq!(
            Some(&a.chain.tip().unwrap().hash),
            lowest.header.selected_parent(),
            "{tag}: retreats to just below the lowest block anchored above the fork point"
        );
        assert!(above.iter().all(|block| !on_main(&a, block)), "{tag}");
    }
    if k <= confs {
        assert!(
            a.chain.is_final(&final_before.0, final_before.1).unwrap(),
            "{tag}: no final block leaves the main chain"
        );
    }
    assert!(
        a.chain.is_final(&history_hash, history_height).unwrap(),
        "{tag}"
    );
    check_anchors(&a, &tag);
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    // a pair still waiting in the BTC mempool keeps its funding inputs reserved, also once its block left the main
    // chain: spending them again would conflict with it, and the producer would discard the block it solved
    let old_inputs = old_pair
        .as_ref()
        .map(|pair| funding_inputs(&pair.commit))
        .unwrap_or_default();
    if content == Content::SettlementWaiting {
        let reserved = a.chain.reserved_inputs().unwrap();
        assert!(old_inputs.iter().all(|i| reserved.contains(i)), "{tag}");
    }

    // the fast lane goes on: the path B op comes back from the entry pool, the native transaction is forced again when
    // it is on the new branch; the next settlement does not spend the funding inputs of a pair still waiting
    let nonce = if content.path_b() { 2 } else { 1 };
    let more = transfer(&alice, nonce, token(2), bob.account, 10);
    a.submit(more.clone());
    let next = a.mine();
    let mut after = vec![next.clone()];
    assert!(includes(&next, &more), "{tag}");
    if k > d && content.path_b() {
        assert!(includes(&next, &path_b), "{tag}: the op comes back");
    }
    if let Some((_, op)) = &native {
        assert_eq!(
            includes(&next, op),
            content == Content::NativeAgain,
            "{tag}: {:?}",
            next.entries
        );
    }
    after.push(a.mine_empty());
    after.push(a.mine_empty());
    a.broadcast_settlements();
    assert!(
        in_mempool(&mock, &pair_of(&next).txid()),
        "{tag}: the next settlement does not conflict with the pairs left behind"
    );
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(
        a.chain
            .carried_on_chain(&next, &snapshot)
            .unwrap()
            .is_some(),
        "{tag}"
    );
    if content == Content::SettlementWaiting {
        // the old pair lands too: with k > d it puts a chain this node left onto BTC, which sets off no republishing
        let pair = old_pair.as_ref().unwrap();
        mock.release(&pair.commit_txid());
        mock.release(&pair.txid());
        mock.mine(1);
        a.sync();
        let snapshot = a.chain.btc().snapshot().unwrap();
        assert!(
            snapshot.header_seen(&content_block.header.hash()).is_some(),
            "{tag}"
        );
        mock.mine(1);
        a.broadcast_settlements();
        let snapshot = a.chain.btc().snapshot().unwrap();
        assert_eq!(
            a.chain.btc_competitor(&snapshot, confs).unwrap(),
            None,
            "{tag}"
        );
        // confirmed, the pair spent them: a block it was sent for that left the main chain holds them no more
        if k > d {
            let reserved = a.chain.reserved_inputs().unwrap();
            assert!(old_inputs.iter().all(|i| !reserved.contains(i)), "{tag}");
        }
    }

    if content == Content::ReorgBack {
        // BTC goes back to the old branch, which grows longer than the new one: the blocks anchored on the new branch
        // leave the main chain and the old ones come back
        let new_branch = mock.tip_height() - fork;
        mock.reorg(new_branch);
        mock.reconnect(&orphaned).unwrap();
        mock.mine(new_branch + 1 - k);
        a.sync();
        assert!(after.iter().all(|block| !on_main(&a, block)), "{tag}");
        assert!(above.iter().all(|block| on_main(&a, block)), "{tag}");
        check_anchors(&a, &tag);
    }

    // everything lands, and finality goes past the retreat
    let last = a.mine();
    if content == Content::ReorgBack {
        assert!(includes(&last, &more), "{tag}: the op comes back again");
    }
    land(&a);
    mock.mine(confs);
    a.sync();
    if k > d {
        assert!(
            a.chain.finalized().unwrap().1 >= above[0].header.height,
            "{tag}: finality goes past the retreat"
        );
    }
    check_anchors(&a, &tag);
    let expected = 1
        + 10
        + if content.path_b() { 7 } else { 0 }
        + if content == Content::NativeAgain {
            4
        } else {
            0
        };
    assert_eq!(ksa(&a, &bob), expected, "{tag}");
    assert_eq!(
        ksa(&a, &carol),
        if content == Content::NativeAgain {
            996
        } else {
            1_000
        },
        "{tag}"
    );

    // the indexer agrees height by height, and so does a node syncing from the genesis block, which takes the blocks
    // left behind as side blocks only
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    let fresh = Side::with_params(&mock, &genesis, &format!("{tag}/fresh"), params);
    import_history(&fresh, &a);
    for block in above
        .iter()
        .chain(&after)
        .filter(|block| !on_main(&a, block))
    {
        let outcome = fresh.chain.accept_block(block, None);
        if content == Content::NativeSpent && includes(block, &native.as_ref().unwrap().1) {
            // its forced-inclusion entry names a native transaction no longer on BTC (D-100)
            assert!(outcome.is_err(), "{tag}");
            break;
        }
        assert_eq!(outcome.unwrap().outcome, Accepted::Side, "{tag}");
    }
    assert_eq!(
        fresh.chain.tip().unwrap().hash,
        a.chain.tip().unwrap().hash,
        "{tag}"
    );
    assert_eq!(ksa(&fresh, &bob), expected, "{tag}");
}

/// Lands the bodies of the blocks produced so far: two empty blocks give them `SETTLEMENT_DEPTH` confirmations, then
/// their settlement transactions are broadcast and confirmed.
fn land(a: &Side) {
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    a.mock.mine(1);
    a.sync();
}

/// The settlement transactions a block carries.
fn pair_of(block: &Block) -> SettlementPair {
    SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap()
}

fn in_mempool(mock: &ksana_btc::mock::MockBtc, txid: &[u8; 32]) -> bool {
    mock.mempool()
        .iter()
        .any(|tx| tx.compute_txid().to_byte_array() == *txid)
}

/// Whether the block includes `op` (path B or forced).
fn includes(block: &Block, op: &VmOp) -> bool {
    block.entries.iter().any(|entry| {
        matches!(entry, Entry::Sponsored { op: included } | Entry::Forced { op: included, .. } if included == op)
    })
}

/// Whether the block is on the side's main chain.
fn on_main(side: &Side, block: &Block) -> bool {
    side.chain.store().hash_at(block.header.height).unwrap() == Some(block.header.hash())
}

/// A transaction spending the native transaction's input elsewhere (the mock chain checks no signatures).
fn rival(native: &Transaction) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: native.input[0].previous_output,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(9_000),
            script_pubkey: ksana_btc::mock::coinbase_script(),
        }],
    }
}

/// No main-chain block above the finalized tip is anchored off the BTC best chain (D-104).
fn check_anchors(side: &Side, tag: &str) {
    let snapshot = side.chain.btc().snapshot().unwrap();
    let off_chain = |anchor: &BtcAnchor| {
        snapshot
            .hash_at(anchor.height)
            .is_some_and(|hash| hash != anchor.hash)
    };
    let (_, final_height) = side.chain.finalized().unwrap();
    for height in final_height + 1..=side.tip_height() {
        let block = side.block(height);
        assert!(
            !off_chain(&block.header.btc_anchor),
            "{tag}: height {height}"
        );
    }
}

/// The KSA balance of `u` at the side's main-chain tip.
fn ksa(side: &Side, u: &User) -> u128 {
    side.chain
        .with_state(|state, _| {
            state
                .get(&keys::balance(&KSA_ASSET_ID, &u.account))
                .unwrap()
                .map_or(0, |v| borsh::from_slice::<u128>(&v).unwrap())
        })
        .unwrap()
}

/// A fresh node takes `a`'s main chain from the genesis block as historical blocks: every block is on its main chain,
/// extending it, or rebuilt from the data on BTC when it was already final as the node started (D-093).
fn import_history(fresh: &Side, a: &Side) {
    for height in 1..=a.tip_height() {
        let block = a.block(height);
        let outcome = fresh.chain.accept_block(&block, None).unwrap().outcome;
        assert!(
            matches!(outcome, Accepted::Extended(_) | Accepted::Duplicate),
            "height {height}: {outcome:?}"
        );
        assert_eq!(
            fresh.block(height).header.hash(),
            block.header.hash(),
            "height {height}"
        );
    }
}

/// A retreat returns the path B ops of every block it takes off the main chain to the entry pool at once (D-041,
/// D-104). With the pool full they compete by standing with the ops already there (M26, D-098), as after any reorg: the
/// retreat does not fail, the pool stays within its cap, and the next block includes what it kept, each op once.
#[test]
fn a_retreat_into_a_full_entry_pool() {
    let tag = "deep-reorg/full-pool";
    let (alice, bob, carol) = (
        user(&format!("{tag}/alice")),
        user(&format!("{tag}/bob")),
        user(&format!("{tag}/carol")),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, tag);
    let cap = 4;
    *a.mempool.lock().unwrap() = Mempool::new(
        PARAMS,
        Policy {
            max_ops: cap,
            ..Policy::default()
        },
    );
    // three blocks with two of alice's ops each, all anchored on the BTC tip X
    mock.mine(1);
    for nonce in 0..6 {
        a.submit(transfer(&alice, nonce, token(3), bob.account, 1));
        if nonce % 2 == 1 {
            assert_eq!(a.mine().entries.len(), 2);
        }
    }
    assert_eq!(ksa(&a, &bob), 6);
    // carol's ops fill the pool
    for nonce in 0..4 {
        a.submit(transfer(&carol, nonce, token(2), bob.account, 1));
    }
    assert_eq!(a.mempool.lock().unwrap().len(), cap);

    // X is reorged out: the node retreats to the genesis block, and alice's six ops come back into the full pool
    mock.reorg(1);
    mock.mine(2);
    a.sync();
    assert_eq!(a.tip_height(), 0);
    assert_eq!(ksa(&a, &bob), 0);
    let kept = a.mempool.lock().unwrap().pending(usize::MAX);
    assert_eq!(kept.len(), cap);
    // alice's returning ops pay more than carol's and take their places, from the start of her nonce chain
    assert!(kept
        .iter()
        .all(|op| op.account == alice.account && op.nonce < 4));
    let block = a.mine();
    assert_eq!(block.entries.len(), cap);
    assert!(kept.iter().all(|op| includes(&block, op)));
    assert_eq!(ksa(&a, &bob), 4);
    assert!(a.mempool.lock().unwrap().is_empty());
}

/// Two producers, one of which sees the BTC reorg late (D-104): while its BTC view still holds the old branch, B builds
/// on the block anchored on it, and A, which saw the reorg, keeps B's block off its main chain. Once B's view catches up
/// B retreats the same way and takes A's chain, and A, B and the indexer agree, with finality going on.
#[test]
fn a_producer_that_sees_the_reorg_late() {
    let tag = "deep-reorg/late";
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, &format!("{tag}/a"));
    let b = Side::new(&mock, &genesis, &format!("{tag}/b"));
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);

    // A's block u, anchored on the BTC tip X, transfers to bob; B takes it
    mock.mine(1);
    let op = transfer(&alice, 0, token(2), bob.account, 5);
    a.submit(op.clone());
    let u = a.mine();
    assert!(includes(&u, &op));
    b.accept(&u);
    assert_eq!(b.chain.tip().unwrap().hash, u.header.hash());

    // X is reorged out; A sees it at once and retreats below u
    mock.reorg(1);
    mock.mine(2);
    a.sync();
    assert_eq!(
        Some(&a.chain.tip().unwrap().hash),
        u.header.selected_parent()
    );
    // B, whose BTC view still holds X, builds v on u without syncing; A keeps v off its main chain
    let v = (0..20)
        .find_map(|_| b.idle.step().unwrap())
        .expect("block production failed");
    assert_eq!(v.header.selected_parent(), Some(&u.header.hash()));
    assert_eq!(v.header.btc_anchor, u.header.btc_anchor);
    assert_eq!(
        a.chain.accept_block(&v, None).unwrap().outcome,
        Accepted::Side
    );
    // A goes on: its next block includes the op again
    let w = a.mine();
    assert!(includes(&w, &op));

    // B's view catches up: it retreats below u as A did and takes A's chain
    b.sync();
    assert_eq!(
        Some(&b.chain.tip().unwrap().hash),
        u.header.selected_parent()
    );
    b.accept(&w);
    assert_eq!(b.chain.tip().unwrap().hash, w.header.hash());
    assert_eq!(ksa(&b, &bob), 5);

    // everything lands, finality goes past the retreat on both nodes, and the indexer agrees
    land(&a);
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    b.sync();
    for side in [&a, &b] {
        check_anchors(side, tag);
        assert!(
            side.chain.finalized().unwrap().1 >= w.header.height,
            "{tag}"
        );
        assert!(!on_main(side, &u) && !on_main(side, &v), "{tag}");
    }
    catch_up(&ix);
    assert_matches_node(&ix, &a);
}

/// A block a retreat leaves behind stays a DAG tip, anchored off the BTC best chain (D-104 item i): Status does not
/// carry it, or a peer that refused it would fetch it again at every Status until it falls out of the merge depth; it
/// carries the main-chain tip the retreat went back to instead (D-104 item k). When BTC goes back to its branch it is
/// carried again, and the block anchored on the branch BTC left is not.
#[test]
fn a_block_a_retreat_left_behind_is_not_advertised() {
    let tag = "deep-reorg/status-tips";
    let (mock, genesis) = setup(&[]);
    let a = Side::new(&mock, &genesis, tag);
    let tips = || a.chain.status_tips(usize::MAX).unwrap();

    // u, anchored on the BTC tip X
    mock.mine(1);
    let u = a.mine();
    assert_eq!(tips(), vec![u.header.hash()]);

    // X is reorged out: the node retreats below u, which stays a DAG tip but is not carried; the main-chain tip, whose
    // only child it is, is
    let orphaned = mock.reorg(1);
    mock.mine(2);
    a.sync();
    assert_eq!(
        Some(&a.chain.tip().unwrap().hash),
        u.header.selected_parent()
    );
    assert_eq!(tips(), vec![a.chain.tip().unwrap().hash]);
    let w = a.mine();
    assert_eq!(tips(), vec![w.header.hash()]);

    // BTC goes back to X's branch, which grows longer: u is carried again and w is not
    mock.reorg(2);
    mock.reconnect(&orphaned).unwrap();
    mock.mine(2);
    a.sync();
    assert_eq!(a.chain.tip().unwrap().hash, u.header.hash());
    assert_eq!(tips(), vec![u.header.hash()]);
}

/// Hands `to` the blocks `from` returns for its locator, in order, as one sync through GetBlocks does, with the outcome
/// of each.
fn get_blocks(from: &Side, to: &Side) -> BTreeMap<[u8; 32], Result<Accepted, NodeError>> {
    to.sync();
    let locator = to.chain.locator().unwrap();
    let mut outcomes = BTreeMap::new();
    for hash in from.chain.sync_hashes(&locator, None, 1_000).unwrap() {
        let block = from.chain.store().block(&hash).unwrap().unwrap();
        let outcome = to.chain.accept_block(&block, None).map(|r| r.outcome);
        outcomes.insert(hash, outcome);
    }
    outcomes
}

/// The message of a block refused for now, `None` for any other outcome.
fn refusal(outcome: Option<&Result<Accepted, NodeError>>) -> Option<&str> {
    match outcome {
        Some(Err(NodeError::Rejected(m))) => Some(m),
        _ => None,
    }
}

/// A node that refuses a block a retreat left behind keeps no block built on it as an orphan (D-104 item j): for the
/// orphan it would fetch the refused block from peers at every sync, and check it again after every BTC block, for as
/// long as that BTC chain stands. Handed over after their parents, as GetBlocks orders them, the blocks built on it are
/// refused with it, from memory the second time; orphans that came first are dropped once it is refused, with the
/// orphans waiting for them. When BTC goes back to their branch, all are taken.
#[test]
fn a_block_built_on_a_refused_block_is_not_kept_as_an_orphan() {
    let tag = "deep-reorg/orphan-parent";
    let (alice, carol, bob) = (
        user(&format!("{tag}/alice")),
        user(&format!("{tag}/carol")),
        user(&format!("{tag}/bob")),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, &format!("{tag}/a"));
    let b = Side::new(&mock, &genesis, &format!("{tag}/b"));
    mock.mine(1);
    let base = a.mine();
    b.accept(&base);

    // The old branch: BTC block X carries carol's native op, u1 forces it next to alice's path B op (so it needs a
    // body), u2 is built on u1 and u3 on u2; b gets none of them
    let (native, forced) = native_tx(&mock, |input| {
        transfer_native(&carol, 0, input, bob.account, 4)
    });
    mock.mine_block(vec![native.clone()]).unwrap();
    a.submit(transfer(&alice, 0, token(2), bob.account, 7));
    let u1 = a.mine();
    assert!(includes(&u1, &forced));
    let u2 = a.mine();
    let u3 = a.mine();
    assert_eq!(u2.header.selected_parent(), Some(&u1.header.hash()));
    assert_eq!(u3.header.selected_parent(), Some(&u2.header.hash()));
    let (h1, h2, h3) = (u1.header.hash(), u2.header.hash(), u3.header.hash());

    // X is reorged out and the native transaction double-spent on the new branch: a retreats below u1
    let orphaned = mock.reorg(1);
    let fork = mock.tip_height();
    mock.mine_block(vec![rival(&native)]).unwrap();
    mock.mine(1);
    a.sync();
    assert_eq!(a.chain.tip().unwrap().hash, base.header.hash());
    let nothing_waits = |round: &str| {
        assert_eq!(b.chain.orphan_count().unwrap(), 0, "{round}");
        assert!(b.chain.missing_parents().unwrap().is_empty(), "{round}");
    };

    // a moves ahead and b syncs from it: GetBlocks hands over u1 to u3 before the new block. b refuses u1 for now, as
    // its forced entry names a transaction the current BTC chain does not have, and u2 and u3 with it
    let w = a.mine();
    let first = get_blocks(&a, &b);
    let m1 = refusal(first.get(&h1)).unwrap();
    assert!(
        m1.contains("is not a native transaction") && !m1.contains(REMEMBERED),
        "{m1}"
    );
    for (hash, parent) in [(h2, h1), (h3, h2)] {
        let m = refusal(first.get(&hash)).unwrap();
        assert!(
            m.contains(&format!(
                "its parent {} is refused for now",
                hex::encode(parent)
            )) && !m.contains(REMEMBERED),
            "{m}"
        );
    }
    assert!(matches!(
        first.get(&w.header.hash()),
        Some(Ok(Accepted::Extended(_)))
    ));
    nothing_waits("first sync");

    // Under the same BTC tip all three are answered from memory
    let again = get_blocks(&a, &b);
    for hash in [h1, h2, h3] {
        let m = refusal(again.get(&hash)).unwrap();
        assert!(m.contains(REMEMBERED), "{m}");
    }
    nothing_waits("second sync");

    // A new BTC tip forgets the refusals. u3 and u2, as by-hash fetches would bring them, wait as orphans until u1
    // comes and is refused again, which drops both
    mock.mine(1);
    b.sync();
    for block in [&u3, &u2] {
        let outcome = b.chain.accept_block(block, None).map(|r| r.outcome);
        assert!(matches!(outcome, Ok(Accepted::Orphan)), "{outcome:?}");
    }
    assert_eq!(b.chain.orphan_count().unwrap(), 2);
    assert!(b.chain.missing_parents().unwrap().contains(&h1));
    let outcome = b.chain.accept_block(&u1, None).map(|r| r.outcome);
    assert!(refusal(Some(&outcome)).is_some(), "{outcome:?}");
    nothing_waits("u1 refused after u3 and u2");

    // BTC and the fast lane move on: nothing waits for u1
    for round in 1..=2 {
        mock.mine(1);
        a.sync();
        let next = a.mine();
        let outcomes = get_blocks(&a, &b);
        for hash in [h1, h2, h3] {
            assert!(refusal(outcomes.get(&hash)).is_some(), "round {round}");
        }
        assert!(
            matches!(
                outcomes.get(&next.header.hash()),
                Some(Ok(Accepted::Extended(_)))
            ),
            "round {round}"
        );
        nothing_waits(&format!("round {round}"));
    }

    // BTC goes back to X's branch, which grows longer: b takes u1 to u3 at the next sync, and follows a onto them
    mock.reorg(mock.tip_height() - fork);
    mock.reconnect(&orphaned).unwrap();
    mock.mine(6);
    a.sync();
    assert_eq!(a.chain.tip().unwrap().hash, h3);
    let back = get_blocks(&a, &b);
    for hash in [h1, h2, h3] {
        assert!(
            matches!(back.get(&hash), Some(Ok(_))),
            "{:?}",
            back.get(&hash)
        );
    }
    assert_eq!(b.chain.tip().unwrap().hash, h3);
    nothing_waits("BTC back");
}

/// A path C op included again runs in the new BTC order, at the start of its block (§7.4, D-104): here ahead of the path
/// B op below it, which comes back through the entry pool. Out of turn, it fails without taking its nonce; the op the
/// wallet signed above it on the old chain waits in the entry pool until the gap is filled, and the indexer agrees.
#[test]
fn a_path_c_op_included_again_can_fail_out_of_turn() {
    let tag = "deep-reorg/out-of-turn";
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, tag);
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);

    // the old branch: alice's path B op of nonce 0 in u, on its first BTC block; on the second, where the native
    // transaction of her path C op of nonce 1 (funded below the fork point) confirmed, that op in v and her path B op of
    // nonce 2 in y
    let (native, path_c) = native_tx(&mock, |input| {
        transfer_native(&alice, 1, input, bob.account, 4)
    });
    mock.mine(1);
    let path_b = transfer(&alice, 0, token(2), bob.account, 7);
    a.submit(path_b.clone());
    let u = a.mine();
    assert!(includes(&u, &path_b));
    mock.mine_block(vec![native]).unwrap();
    let v = a.mine();
    assert!(includes(&v, &path_c));
    let after = transfer(&alice, 2, token(2), bob.account, 5);
    a.submit(after.clone());
    let y = a.mine();
    assert!(includes(&y, &after));
    assert_eq!(ksa(&a, &bob), 16);

    // both BTC blocks are reorged out, and the new branch confirms the native transaction in its first block; alice's
    // wallet signed her next op on the old chain's nonce before the node saw the reorg
    mock.reorg(2);
    let above = transfer(&alice, 3, token(2), bob.account, 10);
    a.submit(above.clone());
    mock.mine(3);
    a.sync();
    assert_eq!(
        Some(&a.chain.tip().unwrap().hash),
        u.header.selected_parent()
    );

    // the next block forces the path C op at its start, ahead of the path B op of nonce 0: out of turn, it fails, and
    // the path B op of nonce 2, back in the entry pool, waits behind the gap with the op above
    let w = a.mine();
    let at = |op: &VmOp| {
        w.entries.iter().position(|entry| {
            matches!(entry, Entry::Sponsored { op: included } | Entry::Forced { op: included, .. } if included == op)
        })
    };
    let (forced, sponsored) = (at(&path_c).unwrap(), at(&path_b).unwrap());
    assert!(forced < sponsored, "{:?}", w.entries);
    assert!(!includes(&w, &after) && !includes(&w, &above));
    let receipts = a
        .chain
        .store()
        .receipts(&w.header.hash())
        .unwrap()
        .unwrap()
        .receipts;
    assert_eq!(
        receipts[forced].status,
        OpStatus::Failed("nonce mismatch".into())
    );
    assert_eq!(receipts[sponsored].status, OpStatus::Success);
    assert_eq!(ksa(&a, &bob), 7);

    // an op for the missing nonce fills the gap, and the two ops waiting above it go through with it
    let fill = transfer(&alice, 1, token(2), bob.account, 4);
    a.submit(fill.clone());
    let x = a.mine();
    assert!(
        includes(&x, &fill) && includes(&x, &after) && includes(&x, &above),
        "{:?}",
        x.entries
    );
    assert_eq!(ksa(&a, &bob), 26);
    assert_eq!(ksa(&a, &alice), 1_000 - 26 - 4 * 2);

    // everything lands, finality goes past the retreat, and the indexer agrees
    land(&a);
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    check_anchors(&a, tag);
    assert!(a.chain.finalized().unwrap().1 >= x.header.height);
    assert!(!on_main(&a, &u) && !on_main(&a, &v) && !on_main(&a, &y));
    catch_up(&ix);
    assert_matches_node(&ix, &a);
}

/// The soak's case (D-104): a retreat returns more path B ops than the entry pool keeps behind a nonce gap, all above a
/// path C op that only the next block includes again. The pool counts the path C op's nonce as filled: it keeps every
/// returned op and admits the op the wallet signs above them (before, it dropped the ninth and refused that one). The
/// next block forces the path C op, the one after includes the path B ops, each once, and the indexer agrees.
#[test]
fn a_retreat_returns_more_ops_above_a_path_c_op_than_a_gap_holds() {
    let tag = "deep-reorg/above-path-c";
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, tag);
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    let gap = a.mempool.lock().unwrap().policy().max_gapped_per_account as u64;

    // the old branch: alice's path C op of nonce 0 in u, on the BTC block where its native transaction confirmed, and
    // her path B ops of nonces 1 to `gap` + 1 in v on top
    let (native, path_c) = native_tx(&mock, |input| {
        transfer_native(&alice, 0, input, bob.account, 4)
    });
    mock.mine_block(vec![native]).unwrap();
    let u = a.mine();
    assert!(includes(&u, &path_c));
    let path_b: Vec<VmOp> = (1..=gap + 1)
        .map(|nonce| transfer(&alice, nonce, token(2), bob.account, 1))
        .collect();
    for op in &path_b {
        a.submit(op.clone());
    }
    let v = a.mine();
    assert!(path_b.iter().all(|op| includes(&v, op)));

    // that BTC block is reorged out and the new branch confirms the native transaction again: the node retreats below
    // u, and the pool keeps every path B op v returns; the op the wallet signs on the old chain's nonce gets in
    mock.reorg(1);
    mock.mine(2);
    a.sync();
    assert_eq!(
        Some(&a.chain.tip().unwrap().hash),
        u.header.selected_parent()
    );
    assert_eq!(a.mempool.lock().unwrap().len(), path_b.len());
    let above = transfer(&alice, gap + 2, token(2), bob.account, 1);
    a.submit(above.clone());

    // the next block forces the path C op, in turn; the one after includes the path B ops
    let w = a.mine();
    assert_eq!(
        outcome_of(&a, &w, &path_c).map(|(_, status)| status),
        Some(OpStatus::Success)
    );
    let x = a.mine();
    assert!(
        path_b.iter().chain([&above]).all(|op| includes(&x, op)),
        "{:?}",
        x.entries
    );
    assert!(a.mempool.lock().unwrap().is_empty());
    assert_eq!(ksa(&a, &bob), 4 + gap as u128 + 2);

    // everything lands, finality goes past the retreat, and the indexer agrees
    land(&a);
    mock.mine(PARAMS.btc_finality_confs);
    a.sync();
    check_anchors(&a, tag);
    assert!(a.chain.finalized().unwrap().1 >= x.header.height);
    assert!(!on_main(&a, &u) && !on_main(&a, &v));
    catch_up(&ix);
    assert_matches_node(&ix, &a);
}

/// Puts a path B op into the side's entry pool; whether the pool took it.
fn admitted(side: &Side, op: &VmOp) -> bool {
    let (_, view) = side.chain.tip_and_view().unwrap();
    let state: Arc<dyn StateSource> = Arc::new(view);
    side.mempool
        .lock()
        .unwrap()
        .insert(op.clone(), &state)
        .is_ok()
}

/// How many of the path B ops `u` signs at nonces `from` to `from + count - 1` the side's entry pool takes.
fn admitted_from(side: &Side, u: &User, from: u64, count: u64, to: &User) -> u64 {
    (from..from + count)
        .map(|nonce| transfer(u, nonce, token(2), to.account, 1))
        .filter(|op| admitted(side, op))
        .count() as u64
}

/// The pool counts as taken only the nonces of the path C ops a block forcing them takes them for (D-104): here four
/// native transactions in one BTC block, in this order: mallory's op of nonce 0 signed over another input than its
/// first, alice's of nonce 1, alice's of nonce 0 and dave's of nonce 0. The block forcing them rejects mallory's as
/// unauthenticated and alice's nonce 1 as out of turn, so the path B ops mallory and alice sign above stay behind a
/// nonce gap, at most `max_gapped_per_account` of them, while dave's form a run. Counted, mallory's nonce would have
/// let her keep ops in the pool that stand as packing candidates and are never packed, for as long as her native
/// transaction waits, at the price of a BTC transaction and no KSA (the audit's F1); alice's nonce 1, counted as a
/// set rather than in BTC order, the same with two valid ops (F2).
#[test]
fn only_the_path_c_ops_a_block_would_take_count_in_the_pool_in_btc_order() {
    let tag = "deep-reorg/forced-nonces";
    let (mallory, alice, dave, bob) = (
        user(&format!("{tag}/mallory")),
        user(&format!("{tag}/alice")),
        user(&format!("{tag}/dave")),
        user(&format!("{tag}/bob")),
    );
    let (mock, genesis) = setup(&[(&mallory, 1_000), (&alice, 1_000), (&dave, 1_000)]);
    let a = Side::new(&mock, &genesis, tag);
    a.mine();
    let gap = a.mempool.lock().unwrap().policy().max_gapped_per_account as u64;

    let elsewhere = mock.fund(&ksana_btc::mock::coinbase_script(), &[10_000])[0];
    let (forged_tx, forged) = native_tx(&mock, |_| {
        transfer_native(&mallory, 0, &elsewhere, bob.account, 1)
    });
    let (late_tx, late) = native_tx(&mock, |input| {
        transfer_native(&alice, 1, input, bob.account, 1)
    });
    let (early_tx, early) = native_tx(&mock, |input| {
        transfer_native(&alice, 0, input, bob.account, 1)
    });
    let (valid_tx, valid) = native_tx(&mock, |input| {
        transfer_native(&dave, 0, input, bob.account, 1)
    });
    mock.mine_block(vec![forged_tx, late_tx, early_tx, valid_tx])
        .unwrap();
    a.sync();

    // gap + 2 path B ops each, above the nonce of their last path C op
    assert_eq!(admitted_from(&a, &dave, 1, gap + 2, &bob), gap + 2);
    assert_eq!(admitted_from(&a, &mallory, 1, gap + 2, &bob), gap);
    assert_eq!(admitted_from(&a, &alice, 2, gap + 2, &bob), gap);

    // the block forcing them agrees, and the next one includes dave's path B ops
    let block = a.mine();
    let status = |op: &VmOp| outcome_of(&a, &block, op).map(|(_, status)| status);
    assert!(
        matches!(status(&forged), Some(OpStatus::Failed(m)) if m.contains("invalid signature")),
        "{:?}",
        status(&forged)
    );
    assert!(
        matches!(status(&late), Some(OpStatus::Failed(m)) if m.contains("nonce mismatch")),
        "{:?}",
        status(&late)
    );
    assert_eq!(status(&early), Some(OpStatus::Success));
    assert_eq!(status(&valid), Some(OpStatus::Success));
    let next = a.mine();
    let sponsored = next
        .entries
        .iter()
        .filter(|entry| matches!(entry, Entry::Sponsored { op } if op.account == dave.account))
        .count() as u64;
    assert_eq!(sponsored, gap + 2);
    assert_eq!(a.mempool.lock().unwrap().len() as u64, 2 * gap);
}

/// The scan for the path C ops waiting for forced inclusion covers the first `NATIVE_CONFIRMATIONS + ANCHOR_DEPTH +
/// BTC_FINALITY_CONFS` heights above the inclusion height of the main-chain tip's anchor (D-104), the heights the next
/// blocks force first: a native transaction above them takes its nonce in the pool only once the main chain gets near
/// it. A node far behind BTC no longer goes through the whole backlog of native transactions for every block it
/// accepts (the audit's F3).
#[test]
fn the_scan_for_path_c_ops_covers_the_heights_the_next_blocks_force() {
    let tag = "deep-reorg/forced-scan";
    let (alice, dave, bob) = (
        user(&format!("{tag}/alice")),
        user(&format!("{tag}/dave")),
        user(&format!("{tag}/bob")),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&dave, 1_000)]);
    let a = Side::new(&mock, &genesis, tag);
    a.mine();
    let gap = a.mempool.lock().unwrap().policy().max_gapped_per_account as u64;

    // BTC moves on while the main chain stays: dave's native transaction lands at the last height the scan covers,
    // alice's at the one above
    let (inside, inside_op) = native_tx(&mock, |input| {
        transfer_native(&dave, 0, input, bob.account, 1)
    });
    let (outside, outside_op) = native_tx(&mock, |input| {
        transfer_native(&alice, 0, input, bob.account, 1)
    });
    let anchor = a.chain.tip().unwrap().header.btc_anchor.height;
    let last = anchor + PARAMS.anchor_depth + PARAMS.btc_finality_confs + 1;
    mock.mine(last - 1 - mock.tip().unwrap().0);
    mock.mine_block(vec![inside]).unwrap();
    mock.mine_block(vec![outside]).unwrap();
    assert_eq!(mock.tip().unwrap().0, last + 1);
    a.sync();
    assert_eq!(a.chain.tip().unwrap().header.btc_anchor.height, anchor);
    assert_eq!(admitted_from(&a, &dave, 1, gap + 2, &bob), gap + 2);
    assert_eq!(admitted_from(&a, &alice, 1, gap + 2, &bob), gap);

    // the next block forces both, and alice's ops above her nonce 0 then form a run
    let block = a.mine();
    assert_eq!(
        outcome_of(&a, &block, &inside_op).map(|(_, status)| status),
        Some(OpStatus::Success)
    );
    assert_eq!(
        outcome_of(&a, &block, &outside_op).map(|(_, status)| status),
        Some(OpStatus::Success)
    );
    assert_eq!(admitted_from(&a, &alice, gap + 1, 2, &bob), 2);
}

/// Mainnet's `ANCHOR_DEPTH` (2) and `NATIVE_CONFIRMATIONS` (3) on the test chain.
fn mainnet_depths() -> Params {
    Params {
        anchor_depth: 2,
        native_confirmations: 3,
        ..PARAMS
    }
}

/// Where `op` is among the entries of the side's main-chain block `block`, and its outcome there.
fn outcome_of(side: &Side, block: &Block, op: &VmOp) -> Option<(usize, OpStatus)> {
    let index = block.entries.iter().position(|entry| {
        matches!(entry, Entry::Sponsored { op: included } | Entry::Forced { op: included, .. } if included == op)
    })?;
    let receipts = side
        .chain
        .store()
        .receipts(&block.header.hash())
        .unwrap()
        .unwrap()
        .receipts;
    Some((index, receipts[index].status.clone()))
}

/// How deep a BTC reorg must be for a path C op included again to fail out of turn (§7.4, §17.3, D-104), with mainnet's
/// depths: deep enough to take back the blocks holding both ops, whether or not it moves the native transaction. Along
/// one chain of blocks the path C op's block anchors at least one BTC block above the path B op's (its window must hold
/// a native transaction its parent's did not), so it takes `ANCHOR_DEPTH` + 2 = 4 blocks; with 3, only the path C op's
/// block is taken back, and the op included again succeeds. The native transaction stays where it was.
#[test]
fn out_of_turn_along_one_chain_takes_anchor_depth_plus_two() {
    assert_eq!(one_chain(3), (OpStatus::Success, false));
    assert_eq!(
        one_chain(4),
        (OpStatus::Failed("nonce mismatch".into()), true)
    );
}

/// The case of [`out_of_turn_along_one_chain_takes_anchor_depth_plus_two`] with a reorg of `k` blocks: the outcome of
/// the path C op included again, and whether the path B op below it came back in the same block.
fn one_chain(k: u32) -> (OpStatus, bool) {
    let params = mainnet_depths();
    let tag = format!("deep-reorg/one-chain/{k}");
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::with_params(&mock, &genesis, &tag, params.clone());
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer_with(dir.path(), &genesis, &mock, params.clone());
    mock.mine(3);
    a.mine();

    // the native transaction of alice's path C op of nonce 1 confirms in the fork point's BTC block F
    let (native, path_c) = native_tx(&mock, |input| {
        transfer_native(&alice, 1, input, bob.account, 4)
    });
    mock.mine_block(vec![native]).unwrap();
    let fork = mock.tip_height();

    // the old branch: u, anchored on F + 1, includes her path B op of nonce 0; v, anchored on F + 2 (F +
    // NATIVE_CONFIRMATIONS - 1), includes the path C op, which succeeds
    mock.mine(3);
    let path_b = transfer(&alice, 0, token(2), bob.account, 7);
    a.submit(path_b.clone());
    let u = a.mine();
    assert_eq!(u.header.btc_anchor.height, fork + 1, "{tag}");
    assert!(includes(&u, &path_b) && !includes(&u, &path_c), "{tag}");
    mock.mine(1);
    let v = a.mine();
    assert_eq!(v.header.btc_anchor.height, fork + 2, "{tag}");
    assert_eq!(
        outcome_of(&a, &v, &path_c).map(|(_, status)| status),
        Some(OpStatus::Success),
        "{tag}"
    );

    // the reorg leaves F, with the native transaction, on the best chain: the main chain retreats below v, and below
    // u too when the reorg takes F + 1
    mock.reorg(k);
    mock.mine(k + 1);
    a.sync();
    let lowest = if k >= 4 { &u } else { &v };
    assert_eq!(
        Some(&a.chain.tip().unwrap().hash),
        lowest.header.selected_parent(),
        "{tag}"
    );

    // the next block includes the path C op again, at its start, ahead of the path B op when that came back
    let w = a.mine();
    let (forced, status) = outcome_of(&a, &w, &path_c).expect("the path C op included again");
    let back = outcome_of(&a, &w, &path_b);
    if let Some((sponsored, path_b_status)) = &back {
        assert!(forced < *sponsored, "{tag}");
        assert_eq!(*path_b_status, OpStatus::Success, "{tag}");
    }

    // everything lands (a block with a body carries the headers onto BTC), finality goes past the retreat, and the
    // indexer agrees
    a.submit(transfer(&bob, 0, token(2), alice.account, 1));
    let last = a.mine();
    land(&a);
    mock.mine(params.btc_finality_confs);
    a.sync();
    check_anchors(&a, &tag);
    assert!(
        a.chain.finalized().unwrap().1 >= last.header.height,
        "{tag}"
    );
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    (status, back.is_some())
}

/// With the path B op in a parallel block merged by the block that included the path C op, a reorg must take the
/// parallel block back too for the path C op to fail out of turn, as the next block merges it again (§7.4, §17.3,
/// D-104 item k, local policy): `ANCHOR_DEPTH` + 2 = 4 blocks with mainnet's depths, as along one chain of blocks.
/// With 3, the shallowest reorg that takes any block back, only the block that merged it goes: the parallel block
/// stays eligible and, although it is no tip (the block taken back is still its child), `Status` carries it and the
/// next block merges it again, blue. Its path B op then runs first, and the path C op included again succeeds after
/// it. A block that did not merge it again would leave the path C op failing after 3 blocks, as before item k.
#[test]
fn out_of_turn_with_a_merged_block_takes_anchor_depth_plus_two() {
    assert_eq!(merged(3), (OpStatus::Success, true));
    assert_eq!(
        merged(4),
        (OpStatus::Failed("nonce mismatch".into()), false)
    );
}

/// The case of [`out_of_turn_with_a_merged_block_takes_anchor_depth_plus_two`] with a reorg of `k` blocks: the outcome
/// of the path C op included again, and whether the next block merged the parallel block again.
fn merged(k: u32) -> (OpStatus, bool) {
    let params = mainnet_depths();
    let tag = format!("deep-reorg/merged/{k}");
    let (alice, bob) = (user(&format!("{tag}/alice")), user(&format!("{tag}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::with_params(&mock, &genesis, &format!("{tag}/a"), params.clone());
    let b = Side::with_params(&mock, &genesis, &format!("{tag}/b"), params.clone());
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer_with(dir.path(), &genesis, &mock, params.clone());
    mock.mine(3);
    let start = a.mine();
    b.accept(&start);

    // the native transaction of alice's path C op of nonce 1 confirms one BTC block below the fork point's block F
    let (native, path_c) = native_tx(&mock, |input| {
        transfer_native(&alice, 1, input, bob.account, 4)
    });
    mock.mine_block(vec![native]).unwrap();
    let fork = mock.tip_height() + 1;
    mock.mine(3);

    // two parallel blocks anchored on F: r from A, empty, and x from B, with her path B op of nonce 0; r, of the
    // empty-block weight, stays A's main chain
    let r = a.mine();
    let path_b = transfer(&alice, 0, token(2), bob.account, 7);
    b.submit(path_b.clone());
    let x = b.mine();
    assert_eq!(r.header.btc_anchor.height, fork, "{tag}");
    assert_eq!(x.header.btc_anchor.height, fork, "{tag}");
    assert_eq!(
        x.header.selected_parent(),
        Some(&start.header.hash()),
        "{tag}"
    );
    assert!(includes(&x, &path_b) && !includes(&x, &path_c), "{tag}");
    a.accept(&x);
    assert_eq!(a.chain.tip().unwrap().hash, r.header.hash(), "{tag}");

    // l, anchored on F + 1, has r as its selected parent, merges x and includes the path C op, which succeeds after
    // the path B op of x
    mock.mine(1);
    let l = a.mine();
    assert_eq!(l.header.btc_anchor.height, fork + 1, "{tag}");
    assert_eq!(l.header.selected_parent(), Some(&r.header.hash()), "{tag}");
    assert!(l.header.parents.contains(&x.header.hash()), "{tag}");
    assert_eq!(
        outcome_of(&a, &l, &path_c).map(|(_, status)| status),
        Some(OpStatus::Success),
        "{tag}"
    );

    // the reorg takes F + 1 off, and F too when it is 4 blocks: the main chain retreats to r, or below r and x;
    // `Status` carries the main-chain tip, and x when it is still eligible
    mock.reorg(k);
    mock.mine(k + 1);
    a.sync();
    let lowest = if k >= 4 { &r } else { &l };
    assert_eq!(
        Some(&a.chain.tip().unwrap().hash),
        lowest.header.selected_parent(),
        "{tag}"
    );
    let mut carried = vec![a.chain.tip().unwrap().hash];
    if k < 4 {
        carried.push(x.header.hash());
    }
    carried.sort_unstable();
    let mut tips = a.chain.status_tips(usize::MAX).unwrap();
    tips.sort_unstable();
    assert_eq!(tips, carried, "{tag}");

    // the next block includes the path C op again, at its start: after the merged x's path B op when it merges x
    // again (blue, so x's block reward counts), else ahead of the path B op, which comes back through the entry pool
    let w = a.mine();
    let again = w.header.parents.contains(&x.header.hash());
    let (forced, status) = outcome_of(&a, &w, &path_c).expect("the path C op included again");
    match outcome_of(&a, &w, &path_b) {
        Some((sponsored, path_b_status)) => {
            assert!(!again && forced < sponsored, "{tag}");
            assert_eq!(path_b_status, OpStatus::Success, "{tag}");
        }
        None => {
            assert!(again, "{tag}");
            let data = a.chain.ghostdag(&w.header.hash()).unwrap().unwrap();
            assert!(data.is_merged_blue(&x.header.hash()), "{tag}");
        }
    }

    // everything lands (a block with a body carries the headers onto BTC: when w merges x again it holds only the
    // forced entry, and x's own settlement, sent once w accepted x, would land alone and make x final instead of r),
    // finality goes past the retreat, and the indexer agrees
    a.submit(transfer(&bob, 0, token(2), alice.account, 1));
    let last = a.mine();
    land(&a);
    mock.mine(params.btc_finality_confs);
    a.sync();
    check_anchors(&a, &tag);
    assert!(
        a.chain.finalized().unwrap().1 >= last.header.height,
        "{tag}"
    );
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    (status, again)
}

/// The boundaries: for every anchor depth, the deepest reorg that changes nothing and the shallowest that takes blocks
/// off the main chain, with a path B op.
#[test]
fn boundaries_of_the_anchor_depth() {
    for d in 0..=3 {
        if d > 0 {
            run(d, d, Content::PathB);
        }
        run(d, d + 1, Content::PathB);
    }
}

/// Every content once, at the shallowest reorg it needs with the devnet depth.
#[test]
fn every_content_at_the_devnet_depth() {
    for content in CONTENTS {
        let k = if content.lands() { 2 } else { 1 };
        run(0, k, content);
    }
}

/// A reorg deeper than `BTC_FINALITY_CONFS` takes a block that was already final (D-082: the decision is revoked
/// first), and BTC going back to the old branch brings it back.
#[test]
fn a_reorg_deeper_than_the_finality_confirmations() {
    run(0, 7, Content::SettlementLanded);
    run(1, 7, Content::ReorgBack);
}

/// The audit's scenario (D-114): B's chain, forking below A's block a1 and heavier on BTC, waits in the BTC mempool
/// while a1's data lands and gets the `BTC_FINALITY_CONFS` confirmations that decide a1 final; then a BTC reorg whose
/// new branch confirms B's settlements first.
///
/// - Of just the BTC block that decided a1, right after the decision: a1's decision is revoked for good, B's chain
///   becomes final instead, and alice's nonce 0 pays carol rather than bob. Node and indexer never reported a1 as
///   BTC-final: that waits for the deciding block's `BTC_FINALITY_CONFS` confirmations (before, they reported the
///   decided tip, and this one-block reorg took it back).
/// - Of one block fewer than `BTC_FINALITY_CONFS` once a1 is reported, which leaves the deciding block in place: a1
///   stays final and reported, also while node and indexer see the shorter new branch on the way.
/// - Of `BTC_FINALITY_CONFS` blocks, the shallowest reaching the deciding block once a1 is reported: it takes a1 back.
#[test]
fn a_reorg_of_the_btc_block_that_decided_a_block_with_a_heavier_chain() {
    let confs = PARAMS.btc_finality_confs;
    deciding_block_reorged(1, false);
    deciding_block_reorged(confs - 1, true);
    deciding_block_reorged(confs, false);
}

/// The case of [`a_reorg_of_the_btc_block_that_decided_a_block_with_a_heavier_chain`] with a reorg of `k` blocks, and
/// node and indexer syncing midway through it or not.
fn deciding_block_reorged(k: u32, midway: bool) {
    let params = PARAMS;
    let confs = params.btc_finality_confs;
    let tag = format!(
        "deep-reorg/deciding/{k}{}",
        if midway { "/midway" } else { "" }
    );
    let name = |who: &str| user(&format!("{tag}/{who}"));
    let (alice, bob, carol) = (name("alice"), name("bob"), name("carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::with_params(&mock, &genesis, &format!("{tag}/a"), params.clone());
    let b = Side::with_params(&mock, &genesis, &format!("{tag}/b"), params.clone());
    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer_with(dir.path(), &genesis, &mock, params.clone());
    let rpc = node_rpc(&a);
    let root = (0, a.block(0).header.hash());

    // B: two blocks with entries, paying carol from alice's nonces 0 and 1, heavier on BTC than A's block 1 alone;
    // their settlements wait in the BTC mempool
    b.submit(transfer(&alice, 0, token(2), carol.account, 20));
    let b1 = b.mine();
    b.submit(transfer(&alice, 1, token(2), carol.account, 5));
    let b2 = b.mine();
    b.mine_empty();
    b.mine_empty();
    b.broadcast_settlements();
    let held: Vec<[u8; 32]> = [&b1, &b2]
        .into_iter()
        .flat_map(|block| {
            let pair = pair_of(block);
            [pair.commit_txid(), pair.txid()]
        })
        .collect();
    for txid in &held {
        assert!(in_mempool(&mock, txid), "{tag}");
        mock.hold(txid);
    }

    // A: block 1 pays bob from alice's nonce 0; its data lands, and it is decided final at the BTC block that gives the
    // data BTC_FINALITY_CONFS confirmations
    let to_bob = transfer(&alice, 0, token(2), bob.account, 10);
    a.submit(to_bob.clone());
    let a1 = a.mine();
    let a1_ref = (1, a1.header.hash());
    land(&a);
    mock.mine(confs - 1);
    let decided_at = mock.tip_height();
    a.sync();
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    assert_eq!(a.chain.finalized().unwrap(), (a1_ref.1, 1), "{tag}");
    let to_bob_state = || rpc.op_status(to_bob.op_id().unwrap()).map(|s| s.status);
    // not reported while the deciding block has fewer confirmations
    for _ in 1..confs {
        assert_eq!(reported(&rpc, &ix), root, "{tag}");
        assert_eq!(to_bob_state().unwrap(), OpState::FastConfirmed, "{tag}");
        if k == 1 {
            break;
        }
        mock.mine(1);
        a.sync();
        catch_up(&ix);
    }
    if k > 1 {
        assert_eq!(reported(&rpc, &ix), a1_ref, "{tag}");
        assert_eq!(to_bob_state().unwrap(), OpState::BtcFinal, "{tag}");
    }

    // the reorg: k blocks off; the new branch's first block confirms B's settlements
    assert_eq!(mock.reorg(k).len(), k as usize, "{tag}");
    let reaches = mock.tip_height() < decided_at;
    assert_eq!(reaches, k == 1 || k >= confs, "{tag}");
    if midway {
        // node and indexer see the shorter chain on the way: a1 stays reported
        a.sync();
        catch_up(&ix);
        assert_eq!(reported(&rpc, &ix), a1_ref, "{tag}");
    }
    for txid in &held {
        mock.release(txid);
    }
    mock.mine(1);
    let first = mock.tip_height();
    for txid in &held {
        assert!(!in_mempool(&mock, txid), "{tag}");
    }
    mock.mine(k);
    a.sync();
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    if !reaches {
        // B's chain connects above the deciding block: too late
        assert_eq!(a.chain.finalized().unwrap(), (a1_ref.1, 1), "{tag}");
        assert_eq!(reported(&rpc, &ix), a1_ref, "{tag}");
        assert_eq!(to_bob_state().unwrap(), OpState::BtcFinal, "{tag}");
        assert_eq!(ksa(&a, &bob), 10, "{tag}");
        return;
    }
    // a1's decision is revoked: with k = 1 nothing was reported, with k = BTC_FINALITY_CONFS a1 is taken back
    assert_ne!(a.chain.finalized().unwrap(), (a1_ref.1, 1), "{tag}");
    assert_eq!(reported(&rpc, &ix), root, "{tag}");
    // B's chain is decided final once its data, landed in the new branch's first block, has BTC_FINALITY_CONFS
    // confirmations, and reported once the BTC block deciding it has as many
    let b_decided = first + confs - 1;
    while mock.tip_height() < b_decided + confs - 2 {
        mock.mine(1);
        a.sync();
    }
    catch_up(&ix);
    assert_matches_node(&ix, &a);
    assert_eq!(a.chain.finalized().unwrap(), (b2.header.hash(), 2), "{tag}");
    assert_eq!(reported(&rpc, &ix), root, "{tag}");
    mock.mine(1);
    a.sync();
    catch_up(&ix);
    assert_eq!(reported(&rpc, &ix), (2, b2.header.hash()), "{tag}");
    assert_matches_node(&ix, &a);
    assert_eq!(ksa(&a, &carol), 25, "{tag}");
    assert_eq!(ksa(&a, &bob), 0, "{tag}");
    assert!(
        !matches!(to_bob_state(), Ok(OpState::BtcFinal)),
        "{tag}: the payment to bob is gone"
    );
    check_anchors(&a, &tag);
}

/// This node's RPC backend (no server started, called directly).
fn node_rpc(side: &Side) -> NodeRpc {
    NodeRpc {
        chain: Arc::clone(&side.chain),
        mempool: Arc::clone(&side.mempool),
        sync: None,
        paused: Arc::new(AtomicBool::new(false)),
        events: side.producer.events.clone(),
        relay: None,
        fees: FeePolicy::new(1),
        calls: AtomicUsize::new(0),
    }
}

/// The BTC-final tip node and indexer report (D-114): `finalized` of the node's `ksana_getChainInfo` and of the
/// indexer's `indexer_status`, which agree.
fn reported(rpc: &NodeRpc, ix: &Indexer) -> (u64, [u8; 32]) {
    let node = rpc.chain_info().unwrap().finalized;
    let indexer = StatusJson::from(ix.status()).finalized;
    assert_eq!(
        (indexer.height, indexer.hash),
        (node.height, node.hash),
        "node and indexer report the same BTC-final tip"
    );
    (node.height, node.hash)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// Random cases beside the sweep: any depth, reorg and content, with extra fast-lane blocks on each BTC block of the
    /// old branch, and the node seeing the shorter BTC chain between the reorg and the new branch or not.
    #[test]
    fn random_reorgs(
        d in 0u32..=3,
        k in 1u32..=8,
        content in 0..CONTENTS.len(),
        extra in 0u32..=2,
        midway in any::<bool>(),
    ) {
        run_timed(d, k, CONTENTS[content], Timing { extra, midway });
    }
}

/// The full sweep: d in 0..=3, k in 1..=d + 2 and 6, 7, every content.
#[test]
#[ignore = "the full sweep, run on demand"]
fn full_sweep() {
    for d in 0..=3 {
        for k in (1..=d + 2).chain([6, 7]) {
            for content in CONTENTS {
                eprintln!("[deep_reorg] d = {d}, k = {k}, {content:?}");
                run(d, k, content);
            }
        }
    }
}
