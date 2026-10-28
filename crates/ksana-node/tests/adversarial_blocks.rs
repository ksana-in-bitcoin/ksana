//! M26 audit: blocks from untrusted peers must not cost the node unbounded memory or CPU. Everything here is local
//! policy: which blocks are valid does not change.
//!
//! - Orphans (blocks whose parents the node does not have yet) wait in memory for their parents. A block that cannot
//!   be valid whatever its parents turn out to be (more than `MAX_BLOCK_PARENTS` parents, a repeated or unordered
//!   parent, PoW not meeting its own `bits`, `bits` above the PoW limit, entries not matching `entries_root`, a body
//!   too large for a payload) is rejected without being kept; the pool is capped by the memory its blocks take as well
//!   as by their count; a legitimate orphan still connects once its parents arrive.
//! - A block whose body does not fit in a payload carrying only its header and body is invalid (D-100, M27).
//! - The bodies a settlement payload carries (D-077) must belong to blocks in the carrying block's past: checking a
//!   body deep in the past costs about the same as checking a recent one, and the outcome of the check is unchanged
//!   for blocks on the main chain, merged blocks, side-chain parents, blocks not in the past and unknown blocks.
//! - A block delivered while syncing and refused for a reason that is not objective (M27) is answered from memory
//!   while the BTC tip stays the same: a refused copy (its settlement transactions) only for that copy, a block
//!   refused for its anchor for every copy; a new BTC tip checks it again. The checks against the BTC view and the
//!   settlement transactions come before the signature checks.

mod common;

use std::mem::size_of;
use std::sync::Arc;
use std::time::Duration;

use bitcoin::hashes::Hash as _;
use bitcoin::{Amount, OutPoint, TxOut, Txid};
use common::side::{setup, Side};
use common::{transfer_op, user};
use ksana_btc::mock::MockBtc;
use ksana_btc::payload::{check_body_fits, entry_data};
use ksana_btc::settlement::{draft, finalize, ProducerUtxo};
use ksana_consensus::{check_pow, Consensus, Sha256d};
use ksana_crypto::{entries_root, sign_vmop};
use ksana_node::chain::{Accepted, Chain, REMEMBERED};
use ksana_node::genesis::GenesisFile;
use ksana_node::NodeError;
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, Block, BlockHeader, Body, Entry, EntryData, FeeMode, Payload, SigScheme, VmOp,
    BLOCK_VERSION, DEVNET, VMOP_VERSION,
};

/// The orphan pool's memory budget the tests hold the node to (32 MiB, the node's `MAX_ORPHAN_BYTES`).
const ORPHAN_BUDGET: usize = 32 * 1024 * 1024;

/// A chain that mines and verifies with the test algorithm Sha256d (D-094), synced with the mock BTC chain.
fn sha_chain(mock: &Arc<MockBtc>, genesis: &GenesisFile) -> (Arc<Chain>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(
        Chain::open_with_algorithm(
            dir.path(),
            DEVNET,
            genesis,
            mock.clone(),
            Some(Arc::new(Sha256d)),
        )
        .unwrap(),
    );
    sync(&chain);
    (chain, dir)
}

/// A node instance like [`Side::new`] (per-block settlement transactions), whose chain uses Sha256d.
fn sha_side(mock: &Arc<MockBtc>, genesis: &GenesisFile, name: &str) -> Side {
    let mut side = Side::new(mock, genesis, name);
    let (chain, dir) = sha_chain(mock, genesis);
    side.producer.chain = chain.clone();
    side.chain = chain;
    side.dir = dir;
    side
}

fn sync(chain: &Chain) {
    while chain.sync_btc().unwrap() {}
}

fn accept(chain: &Chain, block: &Block) -> Result<Accepted, NodeError> {
    sync(chain);
    chain.accept_block(block.clone(), None).map(|r| r.outcome)
}

/// A parent hash nobody has.
fn missing_parent(i: u64) -> [u8; 32] {
    sha256(format!("adversarial/missing/{i}").as_bytes())
}

/// A block on top of `parents` (which the node does not have), with its PoW solved for its own `bits` (the PoW limit).
fn orphan(chain: &Chain, parents: Vec<[u8; 32]>, entries: Vec<Entry>, seed: u64) -> Block {
    let genesis = chain.genesis();
    let mut header = BlockHeader {
        version: BLOCK_VERSION,
        height: 2,
        parents,
        btc_anchor: genesis.btc_anchor.clone(),
        entries_root: entries_root(&entries).unwrap(),
        state_root: genesis.state_root,
        producer: user("adversarial/orphan").account,
        timestamp: genesis.timestamp + seed,
        bits: DEVNET.pow_limit_bits,
        nonce: 0,
    };
    let weight = chain.consensus().weight(&header, 0);
    assert!(chain
        .consensus()
        .solve(&mut header, weight, 0, 1 << 20)
        .unwrap());
    Block {
        header,
        entries,
        settlement: None,
    }
}

/// The same block with a nonce whose PoW does not meet its `bits`.
fn with_bad_pow(chain: &Chain, mut block: Block) -> Block {
    let weight = chain.consensus().weight(&block.header, 0);
    while check_pow(
        chain.consensus().algorithm(),
        &block.header,
        DEVNET.pow_limit_bits,
        weight,
    )
    .is_ok()
    {
        block.header.nonce += 1;
    }
    block
}

/// Path B entries (valid on their own, within `BLOCK_GAS_LIMIT`) too many for one payload with any header: about
/// 100 KiB of ops against the 80 KiB of `MAX_BLOCK_DATA_BYTES`.
/// A body no payload can carry (D-100): two path-B calls with 48 000-byte arguments, within every other limit
/// (`MAX_OP_BYTES`, the gas limit).
fn oversized_body() -> Vec<Entry> {
    let alice = user("adversarial/large/alice");
    (0..2)
        .map(|nonce| {
            let mut op = VmOp {
                version: VMOP_VERSION,
                chain_id: DEVNET.chain_id,
                account: alice.account,
                nonce,
                fee: FeeMode::Token { amount: 1 },
                max_gas: 100_000,
                action: Action::Call {
                    contract: [7; 32],
                    method: "store".into(),
                    args: vec![0; 48_000],
                    funds: Vec::new(),
                },
                sig: SigScheme::Bip340([0; 64]),
            };
            op.sig = sign_vmop(&op, &alice.kp, &[1; 32]).unwrap();
            Entry::Sponsored { op }
        })
        .collect()
}

/// The most unlock entries (heights 0, 1, ...) a body of a block with a header like `header` holds while still fitting
/// in a payload (D-100).
fn most_unlocks_that_fit(header: &BlockHeader) -> u64 {
    let (mut fits, mut too_many) = (1, 100_000);
    while too_many - fits > 1 {
        let mid = (fits + too_many) / 2;
        if check_body_fits(&DEVNET, header, &unlocks(mid, 0)).is_ok() {
            fits = mid;
        } else {
            too_many = mid;
        }
    }
    fits
}

fn unlocks(count: u64, index: u32) -> Vec<Entry> {
    (0..count)
        .map(|height| Entry::Unlock { height, index })
        .collect()
}

/// The memory an orphan holds at least: its encoding (the heap data of its header, entries and settlement
/// transactions) plus the in-memory size of each decoded entry.
fn held(block: &Block) -> usize {
    borsh::object_length(block).unwrap() + block.entries.len() * size_of::<Entry>()
}

#[test]
fn orphans_that_cannot_be_valid_are_not_kept() {
    let (mock, genesis) = setup(&[]);
    let (v, _dir) = sha_chain(&mock, &genesis);
    let entries = unlocks(1, 0);
    let ordered = |n: u64| -> Vec<[u8; 32]> {
        let mut rest: Vec<[u8; 32]> = (1..n).map(missing_parent).collect();
        rest.sort_unstable();
        [vec![missing_parent(0)], rest].concat()
    };
    let (low, high) = {
        let mut pair = [missing_parent(1), missing_parent(2)];
        pair.sort_unstable();
        (pair[0], pair[1])
    };
    let cases: Vec<(&str, Block)> = vec![
        (
            "PoW not meeting its own bits",
            with_bad_pow(&v, orphan(&v, ordered(1), entries.clone(), 1)),
        ),
        ("bits above the PoW limit", {
            let mut block = orphan(&v, ordered(1), entries.clone(), 2);
            block.header.bits = 0x2100_ffff;
            block
        }),
        (
            "more than MAX_BLOCK_PARENTS parents",
            orphan(
                &v,
                ordered(DEVNET.max_block_parents as u64 + 1),
                entries.clone(),
                3,
            ),
        ),
        (
            "ten thousand parents",
            orphan(&v, ordered(10_000), entries.clone(), 4),
        ),
        (
            "a repeated parent",
            orphan(&v, vec![missing_parent(0), low, low], entries.clone(), 5),
        ),
        (
            "the selected parent repeated",
            orphan(
                &v,
                vec![missing_parent(0), missing_parent(0)],
                entries.clone(),
                6,
            ),
        ),
        (
            "parents after the first out of order",
            orphan(&v, vec![missing_parent(0), high, low], entries.clone(), 7),
        ),
        ("entries not matching entries_root", {
            let mut block = orphan(&v, ordered(1), entries.clone(), 8);
            block.entries.push(Entry::Unlock {
                height: 9,
                index: 9,
            });
            block
        }),
        (
            "a body too large for a payload",
            orphan(&v, ordered(1), oversized_body(), 9),
        ),
    ];
    let mut kept = Vec::new();
    for (what, block) in cases {
        let before = v.orphan_count().unwrap();
        let result = accept(&v, &block);
        let stored = v.orphan_count().unwrap() > before;
        eprintln!(
            "{what}: {result:?}{}",
            if stored { ", kept as an orphan" } else { "" }
        );
        if stored || !matches!(result, Err(NodeError::InvalidBlock(_))) {
            kept.push(what);
        }
    }
    let missing = v.missing_parents().unwrap().len();
    eprintln!(
        "{} orphans kept, {missing} missing parents to request",
        v.orphan_count().unwrap()
    );
    assert!(kept.is_empty(), "accepted or kept: {kept:?}");
    assert_eq!(missing, 0);
}

/// D-100 (F-01): a block that needs a body is valid only if a republish payload carrying nothing but its header and
/// body fits in `MAX_BLOCK_DATA_BYTES`; otherwise nobody could land its data on BTC and the blocks after it could never
/// settle. Such a block is invalid however it reaches the node.
#[test]
fn a_body_too_large_for_a_payload_is_invalid() {
    let (mock, genesis) = setup(&[]);
    let (v, _dir) = sha_chain(&mock, &genesis);
    let parent = v.genesis();
    let parent_hash = parent.hash();
    let daa_score = v.ghostdag(&parent_hash).unwrap().unwrap().daa_score + 1;
    let entries = oversized_body();
    let mut header = BlockHeader {
        version: v.params().protocol_version(daa_score),
        height: parent.height + 1,
        parents: vec![parent_hash],
        btc_anchor: parent.btc_anchor.clone(),
        entries_root: entries_root(&entries).unwrap(),
        state_root: parent.state_root,
        producer: user("adversarial/large/producer").account,
        timestamp: parent.timestamp + 1,
        bits: v.consensus().next_bits(parent, daa_score, 1).unwrap(),
        nonce: 0,
    };
    assert!(v.consensus().solve(&mut header, 1, 0, 1 << 20).unwrap());
    let err = check_body_fits(&DEVNET, &header, &entries).unwrap_err();
    assert!(err.contains("does not fit in a payload"), "{err}");
    let block = Block {
        header,
        entries,
        settlement: None,
    };
    let m = invalid(&v, &block);
    assert!(m.contains("does not fit in a payload"), "{m}");
    assert!(v.is_invalid(&block.header.hash()).unwrap());
    assert_eq!(v.tip().unwrap().hash, parent_hash);
}

#[test]
fn the_orphan_pool_is_capped_by_memory() {
    // A decoded entry takes far more memory than its encoding: an unlock entry is 13 bytes encoded
    let unlock = Entry::Unlock {
        height: 1,
        index: 0,
    };
    let unlock_len = borsh::object_length(&unlock).unwrap();
    let alice = user("adversarial/sizes/alice");
    let transfer = Entry::Sponsored {
        op: transfer_op(&alice, 0, 1, alice.account, 1),
    };
    let transfer_len = borsh::object_length(&transfer).unwrap();
    let entry = size_of::<Entry>();
    let gossip = ksana_p2p::wire::MAX_GOSSIP_BYTES;
    eprintln!(
        "size_of::<Entry>() = {entry}, size_of::<Block>() = {}; encoded: unlock entry {unlock_len} bytes, path-B \
         transfer {transfer_len} bytes; a {gossip}-byte block message decodes to about {} MiB of unlock entries or \
         {} MiB of path-B transfers",
        size_of::<Block>(),
        (gossip / unlock_len * (entry + unlock_len)) >> 20,
        (gossip / transfer_len * (entry + transfer_len)) >> 20,
    );

    let (mock, genesis) = setup(&[]);
    let (v, _dir) = sha_chain(&mock, &genesis);
    // An orphan's body must fit in a payload (D-100), which bounds the memory one orphan holds: with the most unlock
    // entries that fit, each takes about a sixth of the budget, so ten do not all fit
    let per_orphan =
        most_unlocks_that_fit(&orphan(&v, vec![missing_parent(0)], unlocks(1, 0), 0).header);
    let blocks: Vec<Block> = (0..10)
        .map(|i| {
            orphan(
                &v,
                vec![missing_parent(i)],
                unlocks(per_orphan, i as u32),
                i,
            )
        })
        .collect();
    assert!(blocks.len() * held(&blocks[0]) > ORPHAN_BUDGET);
    for (i, block) in blocks.iter().enumerate() {
        let result = accept(&v, block);
        assert!(matches!(result, Ok(Accepted::Orphan)), "{i}: {result:?}");
    }
    let count = v.orphan_count().unwrap();
    let total = count * held(&blocks[0]);
    eprintln!(
        "{count} orphans kept, of {} bytes encoded and at least {} bytes in memory each: {total} bytes in all",
        borsh::object_length(&blocks[0]).unwrap(),
        held(&blocks[0])
    );
    assert!(
        total <= ORPHAN_BUDGET,
        "the orphan pool holds {total} bytes ({count} orphans), more than {ORPHAN_BUDGET}"
    );
    // The oldest are dropped first: the newest one is still waiting for its parent
    assert!(v.missing_parents().unwrap().contains(&missing_parent(9)));

    // One unlock entry more and the body no longer fits in a payload: refused before it is kept
    let huge = orphan(
        &v,
        vec![missing_parent(100)],
        unlocks(per_orphan + 1, 100),
        100,
    );
    let result = accept(&v, &huge);
    eprintln!(
        "an orphan of {} bytes in memory whose body is one entry too large for a payload: {result:?}",
        held(&huge)
    );
    assert!(
        matches!(result, Err(NodeError::InvalidBlock(_))),
        "{result:?}"
    );
    assert!(
        !v.missing_parents().unwrap().contains(&missing_parent(100)),
        "an orphan too large for a payload was kept"
    );
    let count = v.orphan_count().unwrap();
    assert!(count * held(&blocks[0]) <= ORPHAN_BUDGET);
}

#[test]
fn a_legitimate_orphan_still_connects() {
    let (alice, bob) = (
        user("adversarial/legit/alice"),
        user("adversarial/legit/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = sha_side(&mock, &genesis, "adversarial-legit-a");
    let (v, _dir) = sha_chain(&mock, &genesis);
    a.submit(transfer_op(&alice, 0, 3, bob.account, 1));
    let first = a.mine();
    let empty = a.mine();
    a.submit(transfer_op(&alice, 1, 3, bob.account, 1));
    let last = a.mine();
    assert!(first.settlement.is_some() && last.settlement.is_some());
    assert!(empty.entries.is_empty());
    // The children arrive before their parents
    assert_eq!(accept(&v, &last).unwrap(), Accepted::Orphan);
    assert_eq!(accept(&v, &empty).unwrap(), Accepted::Orphan);
    assert_eq!(v.orphan_count().unwrap(), 2);
    assert!(v.missing_parents().unwrap().contains(&first.header.hash()));
    let report = v.accept_block(first.clone(), None).unwrap();
    assert!(
        matches!(report.outcome, Accepted::Extended(_)),
        "{:?}",
        report.outcome
    );
    assert_eq!(
        report
            .connected
            .iter()
            .map(|b| b.header.hash())
            .collect::<Vec<_>>(),
        [&first, &empty, &last].map(|b| b.header.hash())
    );
    assert_eq!(v.tip().unwrap().hash, last.header.hash());
    assert_eq!(v.orphan_count().unwrap(), 0);
    assert!(v.missing_parents().unwrap().is_empty());
}

/// With all its parents present, a block's PoW is checked against its own `bits` before GHOSTDAG (second audit, P1-1):
/// GHOSTDAG costs more the longer the merged chains, and a block without valid PoW must not make the node pay for it.
/// Here the second parent is in the past of the first, which GHOSTDAG rejects: with valid PoW that is the reason given;
/// without it, the PoW is, before GHOSTDAG runs.
#[test]
fn a_block_with_known_parents_is_checked_for_pow_before_ghostdag() {
    let alice = user("adversarial/pow-first/alice");
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = sha_side(&mock, &genesis, "adversarial-pow-first");
    let first = a.mine();
    let parents = vec![first.header.hash(), a.chain.genesis().hash()];
    // non-empty blocks: weight 1 whatever the streak
    let valid = orphan(&a.chain, parents.clone(), unlocks(1, 0), 1);
    let err = accept(&a.chain, &valid).unwrap_err();
    assert!(
        matches!(&err, NodeError::InvalidBlock(m) if m.contains("in the past of another parent")),
        "{err:?}"
    );
    let bad = with_bad_pow(&a.chain, orphan(&a.chain, parents, unlocks(1, 0), 2));
    let err = accept(&a.chain, &bad).unwrap_err();
    assert!(
        matches!(&err, NodeError::InvalidBlock(m) if m.contains("PoW target")),
        "{err:?}"
    );
}

/// `block` with settlement transactions carrying `payload`, signed with the producer's funding key but spending an
/// output that does not exist on BTC: every check of the payload runs, then the settlement transactions are rejected
/// (only this copy of the block is, D-041). Each `copy` spends another such output, which makes another copy of the
/// block: a refused copy delivered again under the same BTC tip is answered from memory (M27).
fn with_payload(side: &Side, block: &Block, payload: &Payload, copy: u32) -> Block {
    let funding = side.producer.funding.clone().unwrap();
    let payload = payload.encode(&DEVNET).unwrap();
    assert!(payload.len() <= DEVNET.max_block_data_bytes);
    let utxo = ProducerUtxo {
        outpoint: OutPoint {
            txid: Txid::from_byte_array([7; 32]),
            vout: copy,
        },
        txout: TxOut {
            value: Amount::from_sat(400_000),
            script_pubkey: funding.script.clone(),
        },
    };
    let unsigned = draft(
        &DEVNET,
        &[utxo],
        funding.script.clone(),
        &funding.keypair.x_only_public_key().0,
        &payload,
        2,
    )
    .unwrap();
    let signed = finalize(&DEVNET, unsigned, &payload, &funding.keypair).unwrap();
    Block {
        settlement: Some(signed.encode()),
        ..block.clone()
    }
}

/// A settlement payload for `block` carrying the bodies of `carried` (header and entries), in order.
fn carrying(block: &Block, carried: &[(BlockHeader, Vec<EntryData>)]) -> Payload {
    let mut headers: Vec<BlockHeader> = carried.iter().map(|(h, _)| h.clone()).collect();
    headers.push(block.header.clone());
    let bodies = carried
        .iter()
        .enumerate()
        .map(|(i, (_, entries))| Body {
            header: u32::try_from(i).unwrap(),
            entries: entries.clone(),
        })
        .collect();
    Payload::Settlement {
        headers,
        entries: entry_data(&block.entries),
        bodies,
    }
}

fn body_of(block: &Block) -> (BlockHeader, Vec<EntryData>) {
    (block.header.clone(), entry_data(&block.entries))
}

/// The error message when accepting the block fails as invalid.
fn invalid(chain: &Chain, block: &Block) -> String {
    match accept(chain, block) {
        Err(NodeError::InvalidBlock(m)) => m,
        other => panic!("should be rejected as invalid: {other:?}"),
    }
}

/// The audit finding (M26): the bodies a settlement payload carries are each checked against the carrying block's
/// past by walking down the selected chain, as far as the carried block, once per carried body and parent. A payload
/// carrying the body of one block deep in the chain hundreds of times makes that walk hundreds of times; and since a
/// copy whose settlement transactions are rejected does not make the block invalid, the same copy can be sent again.
/// The deep block stays within `MERGE_DEPTH` of the carrying block: a body below its merge-depth root is rejected
/// outright (D-097).
#[test]
fn a_deep_carried_body_costs_no_more_than_a_recent_one() {
    const DEPTH: usize = 700;
    assert!(u64::try_from(DEPTH).unwrap() + 2 < DEVNET.merge_depth);
    let (alice, bob) = (
        user("adversarial/bodies/alice"),
        user("adversarial/bodies/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = sha_side(&mock, &genesis, "adversarial-bodies-a");
    let (v, _dir) = sha_chain(&mock, &genesis);
    let mut blocks = Vec::new();
    a.submit(transfer_op(&alice, 0, 3, bob.account, 1));
    let deep = a.mine();
    blocks.push(deep.clone());
    for _ in 0..DEPTH {
        blocks.push(a.mine());
    }
    a.submit(transfer_op(&alice, 1, 3, bob.account, 1));
    let recent = a.mine();
    blocks.push(recent.clone());
    blocks.push(a.mine());
    a.submit(transfer_op(&alice, 2, 3, bob.account, 1));
    let carrier = a.mine();
    assert!(carrier.settlement.is_some());
    for block in &blocks {
        accept(&v, block).unwrap();
    }
    assert_eq!(
        v.tip().unwrap().hash,
        carrier.header.parents[0],
        "the victim has every block but the carrying one"
    );

    // As many copies as fit in one payload (MAX_SETTLEMENT_HEADERS, MAX_BLOCK_DATA_BYTES)
    let fits = |copies: usize| {
        [&deep, &recent].iter().all(|b| {
            carrying(&carrier, &vec![body_of(b); copies])
                .encode(&DEVNET)
                .unwrap()
                .len()
                <= DEVNET.max_block_data_bytes
        })
    };
    let mut copies = DEVNET.max_settlement_headers - 1;
    while !fits(copies) {
        copies -= 10;
    }
    assert!(copies >= 200, "only {copies} copies fit in a payload");
    // Another copy each time: the same copy delivered again would be answered from memory (M27)
    let attack = |carried: &Block, copy: u32| {
        with_payload(
            &a,
            &carrier,
            &carrying(&carrier, &vec![body_of(carried); copies]),
            copy,
        )
    };
    let time = |carried: &Block| -> (Duration, String) {
        let mut best = Duration::MAX;
        let mut message = String::new();
        for copy in 0..3 {
            let block = attack(carried, copy);
            let started = ksana_node::monotonic_now();
            let result = v.accept_block(block, None);
            best = best.min(started.elapsed());
            message = match result {
                Err(NodeError::InvalidBlock(m)) => m,
                other => panic!("the copy should be rejected: {other:?}"),
            };
        }
        (best, message)
    };
    let (deep_time, deep_message) = time(&deep);
    let (recent_time, recent_message) = time(&recent);
    eprintln!(
        "{copies} carried bodies: {DEPTH} blocks deep {deep_time:?}, 2 blocks deep {recent_time:?} ({deep_message})"
    );
    // Same outcome: every carried body is in the past and matches, then the settlement input is not on BTC
    for message in [&deep_message, &recent_message] {
        assert!(
            message.contains("not confirmed before the anchor height"),
            "{message}"
        );
    }
    assert!(
        deep_time < recent_time * 2 + Duration::from_millis(50),
        "carrying a deep body {copies} times took {deep_time:?}, a recent one {recent_time:?}"
    );
    // The block itself with its own settlement transactions is fine
    assert!(matches!(accept(&v, &carrier), Ok(Accepted::Extended(_))));
}

/// Which carried bodies are in the carrying block's past is decided as before, for a carrying block whose selected
/// parent is on a side chain: bodies of main-chain blocks below the fork, of a block merged by the main chain and of
/// the side-chain parent are in its past; a main-chain block above the fork and an unknown block are not; a body that
/// differs from the block's entries does not match.
#[test]
fn carried_bodies_are_judged_as_before() {
    let (alice, bob, carol, dave) = (
        user("adversarial/judged/alice"),
        user("adversarial/judged/bob"),
        user("adversarial/judged/carol"),
        user("adversarial/judged/dave"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000), (&dave, 1_000)]);
    let a = sha_side(&mock, &genesis, "adversarial-judged-a");
    let b = sha_side(&mock, &genesis, "adversarial-judged-b");
    let (v, _dir) = sha_chain(&mock, &genesis);
    let share = |block: &Block, sides: &[&Side]| {
        for side in sides {
            side.accept(block).unwrap();
        }
        accept(&v, block).unwrap();
    };
    // x1: on everyone's main chain
    b.submit(transfer_op(&carol, 0, 3, bob.account, 1));
    let x1 = b.mine();
    share(&x1, &[&a]);
    // Two parallel blocks; the next block of A merges the one not on its selected chain
    a.submit(transfer_op(&alice, 0, 3, bob.account, 1));
    b.submit(transfer_op(&carol, 1, 3, bob.account, 1));
    let (p1, p2) = (a.mine(), b.mine());
    share(&p2, &[&a]);
    share(&p1, &[&b]);
    let merging = a.mine();
    assert_eq!(merging.header.parents.len(), 2);
    share(&merging, &[&b]);
    let merged = if merging.header.parents[0] == p1.header.hash() {
        p2.clone()
    } else {
        p1.clone()
    };
    assert_eq!(
        v.accepted_height(&merged.header.hash()).unwrap(),
        Some(merging.header.height)
    );
    // B extends the main chain by two blocks; A builds a lighter branch of two blocks on the same parent
    b.submit(transfer_op(&dave, 0, 3, bob.account, 1));
    let s1 = b.mine();
    let s2 = b.mine();
    a.submit(transfer_op(&alice, 1, 3, bob.account, 1));
    let t1 = a.mine();
    a.submit(transfer_op(&alice, 2, 3, bob.account, 1));
    let t2 = a.mine();
    assert_eq!(t2.header.parents, vec![t1.header.hash()]);
    for block in [&s1, &s2] {
        accept(&v, block).unwrap();
    }
    assert_eq!(accept(&v, &t1).unwrap(), Accepted::Side);
    assert_eq!(v.tip().unwrap().hash, s2.header.hash());

    let judge = |carried: &[(BlockHeader, Vec<EntryData>)]| {
        invalid(&v, &with_payload(&a, &t2, &carrying(&t2, carried), 0))
    };
    // In the past: main-chain blocks below the fork, the merged block (one of p1 and p2), the side-chain parent
    let message = judge(&[body_of(&x1), body_of(&p1), body_of(&p2), body_of(&t1)]);
    assert!(
        message.contains("not confirmed before the anchor height"),
        "{message}"
    );
    // Not in the past: a main-chain block above the fork
    let message = judge(&[body_of(&x1), body_of(&s1)]);
    assert!(message.contains("is not in this block's past"), "{message}");
    assert!(
        message.contains(&hex::encode(s1.header.hash())),
        "{message}"
    );
    // Not in the past: a block the node does not know
    let mut unknown = x1.header.clone();
    unknown.nonce = unknown.nonce.wrapping_add(1);
    let message = judge(&[(unknown.clone(), entry_data(&x1.entries))]);
    assert!(message.contains("is not in this block's past"), "{message}");
    assert!(message.contains(&hex::encode(unknown.hash())), "{message}");
    // In the past but a different body
    let mut tampered = entry_data(&t1.entries);
    tampered.push(EntryData::Unlock {
        height: 1,
        index: 0,
    });
    let message = judge(&[body_of(&x1), (t1.header.clone(), tampered)]);
    assert!(
        message.contains("does not match that block's entries"),
        "{message}"
    );
    assert!(
        message.contains(&hex::encode(t1.header.hash())),
        "{message}"
    );
    // The block itself with its own settlement transactions is accepted
    assert!(accept(&v, &t2).is_ok());
    // Its own payload carried no other body: the main chain is unchanged or switched to the equally long branch
    assert!(v.contains(&t2.header.hash()).unwrap());
}

/// A copy refused while syncing is answered from memory (M27): delivered again under the same BTC tip it is refused
/// without being checked again, and the block is not marked invalid; another copy of the block, with its own
/// settlement transactions, is checked on its own and accepted; a new BTC tip forgets the refusal. A block built on it
/// waits for it as an orphan meanwhile: a copy refused does not drop it, as another copy can be taken (D-104 item j).
#[test]
fn a_refused_copy_is_answered_from_memory_under_the_same_btc_tip() {
    let (alice, bob) = (
        user("adversarial/refusals/alice"),
        user("adversarial/refusals/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = sha_side(&mock, &genesis, "adversarial-refusals-a");
    let (v, _dir) = sha_chain(&mock, &genesis);
    a.submit(transfer_op(&alice, 0, 3, bob.account, 1));
    let block = a.mine();
    let child = a.mine();
    assert!(matches!(accept(&v, &child), Ok(Accepted::Orphan)));
    let wrong = with_payload(&a, &block, &carrying(&block, &[]), 0);
    let refused = "not confirmed before the anchor height";
    let first = invalid(&v, &wrong);
    assert!(
        first.contains(refused) && !first.contains(REMEMBERED),
        "{first}"
    );
    assert_eq!(v.orphan_count().unwrap(), 1);
    for _ in 0..3 {
        let again = invalid(&v, &wrong);
        assert!(
            again.contains(refused) && again.contains(REMEMBERED),
            "{again}"
        );
    }
    assert!(!v.is_invalid(&block.header.hash()).unwrap());
    // Another wrong copy is checked on its own
    let other = invalid(&v, &with_payload(&a, &block, &carrying(&block, &[]), 1));
    assert!(
        other.contains(refused) && !other.contains(REMEMBERED),
        "{other}"
    );
    // A new BTC tip forgets the refusals
    mock.mine(1);
    let after = invalid(&v, &wrong);
    assert!(
        after.contains(refused) && !after.contains(REMEMBERED),
        "{after}"
    );
    // The block with its own settlement transactions is accepted, and the orphan with it
    assert!(matches!(accept(&v, &block), Ok(Accepted::Extended(_))));
    assert!(v.contains(&child.header.hash()).unwrap());
    assert_eq!(v.orphan_count().unwrap(), 0);
}

/// A block refused while syncing for a reason that holds for every copy (here its anchor is above this node's BTC tip)
/// is answered from memory under the same BTC tip, and checked again, and accepted, once the BTC tip changes (M27). A
/// block built on it waits for it as an orphan, whether it comes before the refusal or after it: the refusal is this
/// node's BTC view lagging, not the BTC chain (D-104 item j).
#[test]
fn a_block_refused_for_its_anchor_is_checked_again_on_a_new_btc_tip() {
    let (mock, genesis) = setup(&[]);
    let a = sha_side(&mock, &genesis, "adversarial-refusals-anchor-a");
    let (v, _dir) = sha_chain(&mock, &genesis);
    // A syncs a BTC block V does not have yet and anchors its block there, then builds the next block on it
    mock.mine(1);
    let block = a.mine();
    let child = a.mine();
    assert!(block.header.btc_anchor.height > genesis.btc_anchor.height);
    let orphan = || {
        assert!(matches!(
            v.accept_block(&child, None).map(|r| r.outcome),
            Ok(Accepted::Orphan)
        ));
        assert_eq!(v.orphan_count().unwrap(), 1);
    };
    orphan();
    let deliver = || match v.accept_block(&block, None) {
        Err(NodeError::Btc(m)) => m,
        other => panic!("the block should wait for this node's BTC view: {other:?}"),
    };
    let first = deliver();
    assert!(
        first.contains("above the locally synced BTC tip") && !first.contains(REMEMBERED),
        "{first}"
    );
    assert_eq!(v.orphan_count().unwrap(), 1);
    let again = deliver();
    assert!(again.contains(REMEMBERED), "{again}");
    orphan();
    sync(&v);
    assert!(matches!(
        v.accept_block(&block, None).map(|r| r.outcome),
        Ok(Accepted::Extended(_))
    ));
    assert!(v.contains(&child.header.hash()).unwrap());
    assert_eq!(v.orphan_count().unwrap(), 0);
}

/// The checks against the BTC view and the settlement transactions come before the signature checks (M27): a copy
/// whose settlement transactions do not check out is refused as a copy before its signatures are checked, so the
/// block is not marked invalid; the copy without settlement transactions is checked to the end, and the bad signature
/// makes the block invalid.
#[test]
fn settlement_transactions_are_checked_before_signatures() {
    let (alice, bob) = (
        user("adversarial/order/alice"),
        user("adversarial/order/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = sha_side(&mock, &genesis, "adversarial-order-a");
    let (v, _dir) = sha_chain(&mock, &genesis);
    a.submit(transfer_op(&alice, 0, 3, bob.account, 1));
    let honest = a.mine();
    // The same block with the signature of its path-B op broken, its PoW solved again
    let mut forged = Block {
        settlement: None,
        ..honest.clone()
    };
    let Some(Entry::Sponsored { op }) = forged.entries.last_mut() else {
        panic!("the block has a path-B entry");
    };
    let SigScheme::Bip340(sig) = &mut op.sig else {
        panic!("a BIP340 signature");
    };
    sig[0] ^= 1;
    forged.header.entries_root = entries_root(&forged.entries).unwrap();
    let weight = v.consensus().weight(&forged.header, 0);
    assert!(v
        .consensus()
        .solve(&mut forged.header, weight, 0, 1 << 20)
        .unwrap());
    let hash = forged.header.hash();
    let message = invalid(&v, &with_payload(&a, &forged, &carrying(&forged, &[]), 0));
    assert!(
        message.contains("not confirmed before the anchor height"),
        "{message}"
    );
    assert!(!v.is_invalid(&hash).unwrap());
    let message = invalid(&v, &forged);
    assert!(message.contains("invalid signature"), "{message}");
    assert!(v.is_invalid(&hash).unwrap());
}
