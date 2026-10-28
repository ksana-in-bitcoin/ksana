//! M35: the node's operation index (D-111): every op entry of the blocks the main chain accepts, by position on the
//! main chain (`ksana_getLatestOps`) and by account (`ksana_getAccountOps`: the signer, the receiver of a transfer, the
//! contract called), newest first.
//!
//! - Merged blocks are listed at their place in the acceptance order of the main-chain block merging them; an op
//!   included twice is listed once, where it executed, and a skipped op where it was skipped.
//! - A reorg takes the ops of the disconnected blocks out of the listings in the same write as the main chain.
//! - The index survives a restart, and is built again from the block store for a data directory without it (or with
//!   a build that did not finish).
//! - Over JSON-RPC: statuses, the `height:slot:entry` cursor, paging and parameter checks.

mod common;

use std::path::Path;
use std::time::Duration;

use common::side::{setup, Side};
use common::*;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::rpc_params;
use ksana_btc::mock::MockBtc;
use ksana_node::chain::{Accepted, Chain};
use ksana_node::genesis::GenesisFile;
use ksana_node::store::{ChainStore, IndexedOp, ListedOp, OpOutcome};
use ksana_primitives::{Block, Entry, VmOp, DEVNET};
use ksana_rpc::{codes, KsanaApiClient, OpPosition, OpState};
use ksana_state::rocks::DbMemory;
use serde_json::Value;

/// The whole listing (every op with `None`), newest first, in pages of `limit`; the pages must join up.
fn listing(chain: &Chain, account: Option<&[u8; 32]>, limit: usize) -> Vec<ListedOp> {
    let store = chain.store();
    let mut out: Vec<ListedOp> = Vec::new();
    let mut before = None;
    loop {
        let page = store.list_ops(account, before, limit).unwrap().unwrap();
        assert!(page.ops.len() <= limit);
        out.extend(page.ops);
        match page.next {
            Some(next) => {
                assert_eq!(out.last().map(|op| op.position), Some(next));
                before = Some(next);
            }
            None => break,
        }
    }
    for pair in out.windows(2) {
        assert!(pair[0].position > pair[1].position, "newest first");
    }
    out
}

/// The listing in one page, also checked against the listing two at a time.
fn listed(chain: &Chain, account: Option<&[u8; 32]>) -> Vec<ListedOp> {
    let all = listing(chain, account, 500);
    assert_eq!(listing(chain, account, 2), all);
    all
}

/// The op IDs and outcomes listed.
fn ids(listed: &[ListedOp]) -> Vec<([u8; 32], OpOutcome)> {
    listed
        .iter()
        .map(|op| (op.op.op_id, op.op.outcome))
        .collect()
}

/// The position of `op` in `block`, accepted at `height` in place `slot`.
fn position(block: &Block, height: u64, slot: u32, op: &VmOp) -> OpPosition {
    let entry = block
        .entries
        .iter()
        .position(
            |e| matches!(e, Entry::Sponsored { op: o } | Entry::Forced { op: o, .. } if o == op),
        )
        .expect("the block includes the op");
    OpPosition {
        height,
        slot,
        entry: u32::try_from(entry).unwrap(),
    }
}

fn reopen(
    side: Side,
    mock: &std::sync::Arc<MockBtc>,
    genesis: &GenesisFile,
) -> (Chain, tempfile::TempDir) {
    let Side {
        chain,
        producer,
        dir,
        ..
    } = side;
    drop(producer);
    drop(chain);
    let chain = Chain::open(dir.path(), DEVNET, genesis, mock.clone()).unwrap();
    (chain, dir)
}

/// Two parallel blocks, merged by the next block (the scenario of `m7_dag`): the op both include is listed once,
/// where it executed; of the two ops with the same nonce, the one in the merged block is listed as skipped, at its
/// place in the merging block's acceptance order (the merged block first, the merging block itself last).
#[test]
fn merged_blocks_are_listed_in_acceptance_order_and_each_op_once() {
    let (alice, bob, carol, dave, erin, frank) = (
        user("m35/alice"),
        user("m35/bob"),
        user("m35/carol"),
        user("m35/dave"),
        user("m35/erin"),
        user("m35/frank"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000), (&dave, 1_000)]);
    let a = Side::new(&mock, &genesis, "m35a");
    let b = Side::new(&mock, &genesis, "m35b");
    let duplicate = transfer_op(&alice, 0, 3, bob.account, 10);
    let to_erin = transfer_op(&dave, 0, 2, erin.account, 5);
    let to_frank = transfer_op(&dave, 0, 4, frank.account, 6);
    let extra = transfer_op(&carol, 0, 4, frank.account, 1);
    a.submit(duplicate.clone());
    a.submit(to_erin.clone());
    b.submit(duplicate.clone());
    b.submit(to_frank.clone());
    b.submit(extra.clone());

    let a1 = a.mine();
    let b1 = b.mine();
    assert_eq!(a1.header.parents, b1.header.parents);
    a.accept(&b1).unwrap();
    b.accept(&a1).unwrap();
    let (tip, other) = if a1.header.hash() > b1.header.hash() {
        (a1.clone(), b1.clone())
    } else {
        (b1.clone(), a1.clone())
    };
    let tip_is_a = tip == a1;
    // Before the merge only the main-chain block at height 1 is listed
    for side in [&a, &b] {
        let listed = listed(&side.chain, Some(&alice.account));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].position, position(&tip, 1, 0, &duplicate));
        assert_eq!(listed[0].op.block, tip.header.hash());
    }
    let a2 = a.mine();
    assert_eq!(
        a2.header.parents,
        vec![tip.header.hash(), other.header.hash()]
    );
    let report = b.accept(&a2).unwrap();
    assert!(matches!(report.outcome, Accepted::Extended(_)));

    let (winner, loser) = if tip_is_a {
        (&to_erin, &to_frank)
    } else {
        (&to_frank, &to_erin)
    };
    let winner_at = position(&tip, 1, 0, winner);
    let loser_at = position(&other, 2, 0, loser);
    let extra_at = if tip_is_a {
        position(&other, 2, 0, &extra)
    } else {
        position(&tip, 1, 0, &extra)
    };
    let duplicate_at = position(&tip, 1, 0, &duplicate);
    let id = |op: &VmOp| op.op_id().unwrap();
    for side in [&a, &b] {
        let chain = &side.chain;
        // Every op once, newest first: the other inclusion of the duplicate (in the merged block) is passed over
        let latest = listed(chain, None);
        let mut expected = vec![
            (duplicate_at, id(&duplicate), OpOutcome::Success),
            (winner_at, id(winner), OpOutcome::Success),
            (loser_at, id(loser), OpOutcome::Skipped),
            (extra_at, id(&extra), OpOutcome::Success),
        ];
        expected.sort_by_key(|x| std::cmp::Reverse(x.0));
        assert_eq!(
            latest
                .iter()
                .map(|op| (op.position, op.op.op_id, op.op.outcome))
                .collect::<Vec<_>>(),
            expected
        );
        for op in &latest {
            let block = if op.position.height == 1 {
                &tip
            } else {
                &other
            };
            assert_eq!(op.op.block, block.header.hash());
            // The position the `ops` index gives the op
            let location = chain.store().op_location(&op.op.op_id).unwrap().unwrap();
            assert_eq!(location.block, op.op.block);
            assert_eq!(location.height, op.position.height);
            assert_eq!(location.index, op.position.entry);
        }
        // By account: the signer and the receiver
        assert_eq!(
            ids(&listed(chain, Some(&alice.account))),
            [(id(&duplicate), OpOutcome::Success)]
        );
        assert_eq!(
            ids(&listed(chain, Some(&bob.account))),
            [(id(&duplicate), OpOutcome::Success)]
        );
        assert_eq!(
            ids(&listed(chain, Some(&dave.account))),
            [
                (id(loser), OpOutcome::Skipped),
                (id(winner), OpOutcome::Success)
            ]
        );
        assert_eq!(
            ids(&listed(chain, Some(&carol.account))),
            [(id(&extra), OpOutcome::Success)]
        );
        let erin_listed = ids(&listed(chain, Some(&erin.account)));
        let erin_outcome = if tip_is_a {
            OpOutcome::Success
        } else {
            OpOutcome::Skipped
        };
        assert_eq!(erin_listed, [(id(&to_erin), erin_outcome)]);
        let frank_listed = listed(chain, Some(&frank.account));
        assert_eq!(frank_listed.len(), 2);
        assert!(frank_listed.iter().any(|op| op.op.op_id == id(&extra)));
        assert!(frank_listed.iter().any(|op| op.op.op_id == id(&to_frank)));
        assert!(listed(chain, Some(&user("m35/nobody").account)).is_empty());
        let page = chain.store().list_ops(None, None, 1).unwrap().unwrap();
        assert_eq!(page.tip, (a2.header.hash(), 2));
    }

    // After a restart the index is still current and lists the same
    let before = listed(&a.chain, None);
    let (reopened, _dir) = reopen(a, &mock, &genesis);
    assert!(reopened.store().index_current().unwrap());
    assert_eq!(listed(&reopened, None), before);
}

/// A reorg (the scenario of `m4_fork`): the ops of the disconnected blocks leave the listings with the main chain, and
/// when the next block merges the old branch, its conflicting op comes back as skipped, at the new height.
#[test]
fn a_reorg_takes_the_disconnected_ops_out_of_the_listings() {
    let (alice, bob, carol) = (user("m35/r/alice"), user("m35/r/bob"), user("m35/r/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m35r/a");
    let b = Side::new(&mock, &genesis, "m35r/b");
    let to_bob = transfer_op(&alice, 0, 3, bob.account, 10);
    let to_carol = transfer_op(&alice, 0, 4, carol.account, 20);
    a.submit(to_bob.clone());
    b.submit(to_carol.clone());
    let a_blocks: Vec<Block> = (0..2).map(|_| a.mine()).collect();
    let b_blocks: Vec<Block> = (0..4).map(|_| b.mine()).collect();
    assert_eq!(
        ids(&listed(&a.chain, Some(&bob.account))),
        [(to_bob.op_id().unwrap(), OpOutcome::Success)]
    );
    for block in &b_blocks {
        a.accept(block).unwrap();
    }
    assert_eq!(a.chain.tip().unwrap().hash, b_blocks[3].header.hash());
    assert!(listed(&a.chain, Some(&bob.account)).is_empty());
    let carol_at = position(&b_blocks[0], 1, 0, &to_carol);
    for account in [None, Some(&alice.account), Some(&carol.account)] {
        let listed = listed(&a.chain, account);
        assert_eq!(listed.len(), 1, "{account:?}");
        assert_eq!(listed[0].position, carol_at);
        assert_eq!(listed[0].op.op_id, to_carol.op_id().unwrap());
    }
    let page = a.chain.store().list_ops(None, None, 1).unwrap().unwrap();
    assert_eq!(page.tip, (b_blocks[3].header.hash(), 4));

    // The next block merges A's old branch: the transfer to bob is skipped there (nonce 0 is spent)
    let next = a.mine();
    assert_eq!(next.header.height, 5);
    let merged = a.chain.main_chain().unwrap();
    let entry = merged.get(5).unwrap();
    let slot = entry
        .accepted()
        .position(|s| s.hash == a_blocks[0].header.hash())
        .expect("the old branch is merged");
    let bob_listed = listed(&a.chain, Some(&bob.account));
    assert_eq!(
        ids(&bob_listed),
        [(to_bob.op_id().unwrap(), OpOutcome::Skipped)]
    );
    assert_eq!(
        bob_listed[0].position,
        position(&a_blocks[0], 5, u32::try_from(slot).unwrap(), &to_bob)
    );
    assert_eq!(listed(&a.chain, Some(&alice.account)).len(), 2);
}

/// Opens the block store's RocksDB under `chain_dir` with all of its column families.
fn raw_db(chain_dir: &Path) -> rocksdb::DB {
    let names = rocksdb::DB::list_cf(&rocksdb::Options::default(), chain_dir).unwrap();
    rocksdb::DB::open_cf(&rocksdb::Options::default(), chain_dir, names).unwrap()
}

/// A data directory of a version without the index (no `op_seq` / `account_ops`, no marker), and one whose build did
/// not finish (a stale entry left, no marker): the node builds the index again at startup from the block store, the
/// same as it kept it.
#[test]
fn the_index_is_built_again_for_a_data_directory_without_it() {
    let (alice, bob, carol, dave) = (
        user("m35/b/alice"),
        user("m35/b/bob"),
        user("m35/b/carol"),
        user("m35/b/dave"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 1_000)]);
    let a = Side::new(&mock, &genesis, "m35b/a");
    let b = Side::new(&mock, &genesis, "m35b/b");
    // Parallel blocks both including one op, merged by the next block, then a few more ops
    let duplicate = transfer_op(&alice, 0, 3, bob.account, 10);
    a.submit(duplicate.clone());
    b.submit(duplicate);
    b.submit(transfer_op(&carol, 0, 3, dave.account, 7));
    let a1 = a.mine();
    let b1 = b.mine();
    a.accept(&b1).unwrap();
    b.accept(&a1).unwrap();
    a.mine();
    a.submit(transfer_op(&alice, 1, 3, carol.account, 1));
    a.submit(transfer_op(&carol, 1, 3, alice.account, 2));
    a.mine();
    a.mine();
    let accounts = [
        None,
        Some(alice.account),
        Some(bob.account),
        Some(carol.account),
        Some(dave.account),
    ];
    let kept: Vec<Vec<ListedOp>> = accounts
        .iter()
        .map(|account| listed(&a.chain, account.as_ref()))
        .collect();
    assert_eq!(
        kept[0].len(),
        4,
        "five inclusions, one of them skipped where the op executed elsewhere"
    );
    let (chain, dir) = reopen(a, &mock, &genesis);
    drop(chain);
    let chain_dir = dir.path().join("chain");

    // A version without the index (nor the standards index of D-120 beside it)
    {
        let mut db = raw_db(&chain_dir);
        for name in ["op_seq", "account_ops", "standards", "standards_undo"] {
            db.drop_cf(name).unwrap();
        }
        let meta = db.cf_handle("meta").unwrap();
        db.delete_cf(meta, b"index").unwrap();
    }
    {
        let store = ChainStore::open(&chain_dir, &DbMemory::new()).unwrap();
        assert!(!store.index_current().unwrap());
        assert_eq!(store.list_ops(None, None, 50).unwrap(), None);
    }
    let chain = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert!(chain.store().index_current().unwrap());
    for (account, kept) in accounts.iter().zip(&kept) {
        assert_eq!(&listed(&chain, account.as_ref()), kept);
    }
    drop(chain);

    // A build that did not finish: a stale entry above the tip, under an account of its own, and no marker
    let stray = [0x5a; 32];
    {
        let db = raw_db(&chain_dir);
        let mut key = Vec::new();
        key.extend_from_slice(&99u64.to_be_bytes());
        key.extend_from_slice(&0u32.to_be_bytes());
        key.extend_from_slice(&0u32.to_be_bytes());
        let stale = IndexedOp {
            op_id: [0x5b; 32],
            block: [0x5c; 32],
            outcome: OpOutcome::Success,
            accounts: vec![stray],
        };
        db.put_cf(
            db.cf_handle("op_seq").unwrap(),
            &key,
            borsh::to_vec(&stale).unwrap(),
        )
        .unwrap();
        let mut account_key = stray.to_vec();
        account_key.extend_from_slice(&key);
        db.put_cf(db.cf_handle("account_ops").unwrap(), account_key, [])
            .unwrap();
        db.delete_cf(db.cf_handle("meta").unwrap(), b"index")
            .unwrap();
    }
    let chain = Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap();
    assert!(chain.store().index_current().unwrap());
    assert!(listed(&chain, Some(&stray)).is_empty());
    for (account, kept) in accounts.iter().zip(&kept) {
        assert_eq!(&listed(&chain, account.as_ref()), kept);
    }
}

/// The JSON-RPC methods on a running node: statuses, the cursor, paging, and refusals of malformed parameters; the
/// index survives a restart of the process.
#[test]
fn listings_over_json_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let btc = TestBtc::new(dir.path());
    let (alice, bob, carol, producer) = (
        user("m35/rpc/alice"),
        user("m35/rpc/bob"),
        user("m35/rpc/carol"),
        user("m35/rpc/producer"),
    );
    let genesis = write_genesis(dir.path(), btc.anchor(), &[(&alice, 1_000_000)], None);
    let data = dir.path().join("data");
    let node = NodeProcess::start(&data, &genesis, Some(&producer), 2, &btc);
    let rt = rt();
    let c = node.client();
    wait_height(&rt, &c, 1, Duration::from_secs(120));
    let ops = [
        transfer_op(&alice, 0, 5, bob.account, 100),
        transfer_op(&alice, 1, 5, carol.account, 200),
        transfer_op(&alice, 2, 5, bob.account, 300),
    ];
    for op in &ops {
        let op_id = submit(&rt, &c, op).unwrap();
        let status = wait_included(&rt, &c, op_id, Duration::from_secs(120));
        assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    }
    let ids: Vec<[u8; 32]> = ops.iter().map(|op| op.op_id().unwrap()).collect();
    let hex_of = |u: &User| hex::encode(u.account);

    let all = rt
        .block_on(c.get_account_ops(hex_of(&alice), None, None))
        .unwrap();
    assert_eq!(
        all.ops.iter().map(|op| op.op_id).collect::<Vec<_>>(),
        [ids[2], ids[1], ids[0]]
    );
    assert_eq!(all.next, None);
    assert!(all.at.height >= all.ops[0].block.height);
    for (op, expected) in all.ops.iter().zip(ids.iter().rev()) {
        let status = rt.block_on(c.get_op_status(hex::encode(expected))).unwrap();
        assert_eq!(Some(op.block.clone()), status.block);
        assert_eq!(Some(op.index), status.index);
        assert!(matches!(
            op.status,
            OpState::FastConfirmed | OpState::BtcFinal
        ));
    }

    // Bob, one at a time
    let first = rt
        .block_on(c.get_account_ops(hex_of(&bob), None, Some(1)))
        .unwrap();
    assert_eq!(first.ops.len(), 1);
    assert_eq!(first.ops[0].op_id, ids[2]);
    let next = first.next.expect("one more below");
    let second = rt
        .block_on(c.get_account_ops(hex_of(&bob), Some(next), Some(1)))
        .unwrap();
    assert_eq!(
        second.ops.iter().map(|op| op.op_id).collect::<Vec<_>>(),
        [ids[0]]
    );
    assert_eq!(second.next, None);
    let carol_ops = rt
        .block_on(c.get_account_ops(hex_of(&carol), None, None))
        .unwrap();
    assert_eq!(
        carol_ops.ops.iter().map(|op| op.op_id).collect::<Vec<_>>(),
        [ids[1]]
    );
    let nobody = rt
        .block_on(c.get_account_ops(hex_of(&user("m35/rpc/nobody")), None, None))
        .unwrap();
    assert!(nobody.ops.is_empty());
    assert_eq!(nobody.next, None);

    // The whole chain, two at a time
    let latest = rt.block_on(c.get_latest_ops(None, Some(2))).unwrap();
    assert_eq!(
        latest.ops.iter().map(|op| op.op_id).collect::<Vec<_>>(),
        [ids[2], ids[1]]
    );
    let rest = rt.block_on(c.get_latest_ops(latest.next, Some(2))).unwrap();
    assert_eq!(
        rest.ops.iter().map(|op| op.op_id).collect::<Vec<_>>(),
        [ids[0]]
    );
    assert_eq!(rest.next, None);

    // The JSON form: the cursor is a "height:slot:entry" string, heights are decimal strings
    let raw: Value = rt
        .block_on(c.request("ksana_getLatestOps", rpc_params![Value::Null, 1]))
        .unwrap();
    let cursor = raw["next"].as_str().unwrap();
    assert_eq!(OpPosition::parse(cursor).unwrap(), latest.ops[0].position);
    assert!(raw["ops"][0]["block"]["height"].is_string());
    assert!(raw["ops"][0]["index"].is_u64());
    assert!(raw["at"]["height"].is_string());
    let by_cursor: Value = rt
        .block_on(c.request(
            "ksana_getAccountOps",
            rpc_params![hex_of(&alice), cursor, 5],
        ))
        .unwrap();
    assert_eq!(by_cursor["ops"].as_array().unwrap().len(), 2);

    // Malformed parameters
    for params in [
        rpc_params![hex_of(&alice), Value::Null, 0],
        rpc_params![hex_of(&alice), Value::Null, 501],
        rpc_params![hex_of(&alice), "7:0"],
        rpc_params![hex_of(&alice), "07:0:0"],
        rpc_params!["zz"],
    ] {
        let refused = rt
            .block_on(c.request::<Value, _>("ksana_getAccountOps", params))
            .unwrap_err();
        assert_eq!(error_code(&refused), codes::INVALID_PARAMS, "{refused}");
    }

    // Once BTC blocks bury the settlements, the listings say BTC final, as ksana_getOpStatus does
    let status_of = |id: &[u8; 32]| {
        rt.block_on(c.get_op_status(hex::encode(id)))
            .unwrap()
            .status
    };
    let started = ksana_node::monotonic_now();
    while status_of(&ids[2]) != OpState::BtcFinal {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "timed out waiting for BTC finality"
        );
        btc.fake.mock.mine(1);
        std::thread::sleep(Duration::from_millis(300));
    }
    let finals = rt
        .block_on(c.get_account_ops(hex_of(&alice), None, None))
        .unwrap();
    assert_eq!(finals.ops.len(), 3);
    for op in &finals.ops {
        assert_eq!(op.status, OpState::BtcFinal, "{op:?}");
        assert_eq!(status_of(&op.op_id), OpState::BtcFinal);
    }

    // After a restart the same
    node.stop();
    let node = NodeProcess::start(&data, &genesis, None, 2, &btc);
    let c = node.client();
    let again = rt
        .block_on(c.get_account_ops(hex_of(&alice), None, None))
        .unwrap();
    assert_eq!(
        again.ops.iter().map(|op| op.op_id).collect::<Vec<_>>(),
        [ids[2], ids[1], ids[0]]
    );
}
