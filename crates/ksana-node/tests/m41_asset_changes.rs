//! M41: the changes to built-in assets in the node's receipts (D-122).
//!
//! - `ksana_getOpStatus`, `ksana_getBlock` and `ksana_call` give each receipt's changes, in their JSON form.
//! - The operation listings (D-111) also list an op under the accounts its changes name and under each asset but KSA,
//!   so that an asset's own listing is its history; a deployment stays off the listing of the contract it deploys.
//! - Receipts a version before D-122 stored read back with `asset_changes` `null`, and an index built again from them
//!   lists their ops as that version did.
//!
//! The issuing contract is the test contract `probe`, whose methods anyone may call.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use borsh::BorshSerialize;
use common::side::{setup, Side};
use common::{user, User};
use ksana_crypto::sign_vmop;
use ksana_execution::builtin::{asset_id, contract_id};
use ksana_execution::{Event, OpStatus, Receipt};
use ksana_mempool::{Mempool, Policy};
use ksana_node::chain::Chain;
use ksana_node::producer::FeePolicy;
use ksana_node::rpc::NodeRpc;
use ksana_node::store::RECEIPTS_MAGIC;
use ksana_primitives::asset::{CAP_FREEZE, CAP_PAUSE};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, FeeMode, Funds, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};
use ksana_rpc::{AssetChangeJson, Backend, BlockId, CallRequestJson, ReceiptJson};
use ksana_test_contracts::PROBE;
use serde_json::json;
use tokio::sync::broadcast;

fn signed(u: &User, nonce: u64, action: Action) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::Token { amount: 10 },
        max_gas: 3_000_000,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[5; 32]).unwrap();
    op
}

fn call(contract: [u8; 32], method: &str, args: impl BorshSerialize) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args: borsh::to_vec(&args).unwrap(),
        funds: Vec::new(),
    }
}

/// An RPC backend of `chain` with an entry pool of its own: the queries here read the chain alone.
fn rpc_for(chain: &Arc<Chain>) -> NodeRpc {
    NodeRpc {
        chain: Arc::clone(chain),
        mempool: Arc::new(Mutex::new(Mempool::new(DEVNET, Policy::default()))),
        sync: None,
        paused: Arc::new(AtomicBool::new(false)),
        events: broadcast::channel(16).0,
        relay: None,
        fees: FeePolicy::new(1),
        calls: AtomicUsize::new(0),
    }
}

/// The receipt of an op that succeeded.
fn receipt(rpc: &NodeRpc, op: &VmOp) -> ReceiptJson {
    let status = rpc.op_status(op.op_id().unwrap()).unwrap();
    let receipt = status.receipt.expect("included");
    assert!(receipt.success, "{:?}", receipt.error);
    receipt
}

/// The block that includes `op`.
fn block_of(rpc: &NodeRpc, op: &VmOp) -> [u8; 32] {
    let status = rpc.op_status(op.op_id().unwrap()).unwrap();
    status.block.expect("included").hash
}

/// The ops listed under `account`, by ID, newest first.
fn listed(rpc: &NodeRpc, account: [u8; 32]) -> Vec<[u8; 32]> {
    rpc.account_ops(account, None, 500)
        .unwrap()
        .ops
        .into_iter()
        .map(|op| op.op_id)
        .collect()
}

fn ids(ops: &[&VmOp]) -> Vec<[u8; 32]> {
    ops.iter().map(|op| op.op_id().unwrap()).collect()
}

/// Alice deploys the probe with 1 000 KSA, which creates an asset that can be frozen and paused and issues 100 of it
/// to her; she sends Bob 30 of it, the probe freezes him and pays him 7 KSA, and she sends him 5 KSA. Then she
/// deploys a second probe; the first issues a second asset, created with `mint` alone, to Bob and to the second probe,
/// and makes the second probe burn 2 of it.
struct Scenario {
    side: Side,
    mock: Arc<ksana_btc::mock::MockBtc>,
    genesis: ksana_node::genesis::GenesisFile,
    alice: User,
    bob: User,
    probe: [u8; 32],
    asset: [u8; 32],
    /// The second probe.
    burner: [u8; 32],
    /// The second asset.
    minted: [u8; 32],
    ops: Vec<VmOp>,
}

fn scenario(name: &str) -> Scenario {
    let (alice, bob) = (user(&format!("{name}/alice")), user(&format!("{name}/bob")));
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let side = Side::new(&mock, &genesis, name);
    let probe = contract_id(&alice.account, 0, &sha256(PROBE));
    let sub_id = [6u8; 32];
    let asset = asset_id(&probe, &sub_id);
    let meta = ("Dollar".to_string(), "USDX".to_string(), 6u8);
    let burner = contract_id(&alice.account, 6, &sha256(PROBE));
    let minted_sub_id = [8u8; 32];
    let minted = asset_id(&probe, &minted_sub_id);
    let minted_meta = ("Point".to_string(), "PNT".to_string(), 0u8);
    let actions = [
        Action::Deploy {
            code: PROBE.to_vec(),
            init_args: borsh::to_vec(&b"ok".to_vec()).unwrap(),
            funds: vec![Funds {
                asset: KSA_ASSET_ID,
                amount: 1_000,
            }],
        },
        call(
            probe,
            "create_asset",
            (sub_id, meta, CAP_FREEZE | CAP_PAUSE, alice.account, 100u128),
        ),
        Action::AssetTransfer {
            asset,
            to: bob.account,
            amount: 30,
        },
        call(probe, "freeze", (sub_id, bob.account, true)),
        call(probe, "send", (KSA_ASSET_ID, bob.account, 7u128)),
        Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: bob.account,
            amount: 5,
        },
        Action::Deploy {
            code: PROBE.to_vec(),
            init_args: borsh::to_vec(&b"ok".to_vec()).unwrap(),
            funds: Vec::new(),
        },
        call(
            probe,
            "issue_meta",
            (minted_sub_id, minted_meta.clone(), bob.account, 10u128),
        ),
        call(
            probe,
            "issue_meta",
            (minted_sub_id, minted_meta, burner, 5u128),
        ),
        call(
            probe,
            "forward",
            (
                burner,
                "burn".to_string(),
                borsh::to_vec(&(minted, 2u128)).unwrap(),
                Vec::<Funds>::new(),
            ),
        ),
    ];
    let mut ops = Vec::new();
    for (nonce, action) in (0u64..).zip(actions) {
        let op = signed(&alice, nonce, action);
        side.submit(op.clone());
        assert_eq!(side.mine().entries.len(), 1, "one block for each op");
        ops.push(op);
    }
    Scenario {
        side,
        mock,
        genesis,
        alice,
        bob,
        probe,
        asset,
        burner,
        minted,
        ops,
    }
}

#[test]
fn receipts_and_listings_follow_the_changes_to_built_in_assets() {
    let s = scenario("m41-changes/a");
    let rpc = rpc_for(&s.side.chain);
    let (alice, bob, probe, asset) = (s.alice.account, s.bob.account, s.probe, s.asset);
    let (burner, minted) = (s.burner, s.minted);
    let [deploy, create, sent, freeze, paid, ksa, deployed, to_bob, to_burner, burned] = &s.ops[..]
    else {
        panic!("ten ops")
    };

    let expected = [
        (
            deploy,
            vec![AssetChangeJson::Funds {
                asset: KSA_ASSET_ID,
                from: alice,
                to: probe,
                amount: 1_000,
                events: 0,
            }],
        ),
        (
            create,
            vec![
                AssetChangeJson::Create {
                    asset,
                    capabilities: vec!["freeze".into(), "pause".into()],
                    events: 0,
                },
                AssetChangeJson::Issue {
                    asset,
                    to: alice,
                    amount: 100,
                    events: 0,
                },
            ],
        ),
        (
            sent,
            vec![AssetChangeJson::Transfer {
                asset,
                from: alice,
                to: bob,
                amount: 30,
                events: 0,
            }],
        ),
        (
            freeze,
            vec![AssetChangeJson::Freeze {
                asset,
                account: bob,
                frozen: true,
                events: 0,
            }],
        ),
        (
            paid,
            vec![AssetChangeJson::Payout {
                asset: KSA_ASSET_ID,
                from: probe,
                to: bob,
                amount: 7,
                events: 0,
            }],
        ),
        (
            ksa,
            vec![AssetChangeJson::Transfer {
                asset: KSA_ASSET_ID,
                from: alice,
                to: bob,
                amount: 5,
                events: 0,
            }],
        ),
        (deployed, Vec::new()),
        (
            to_bob,
            vec![
                AssetChangeJson::Create {
                    asset: minted,
                    capabilities: vec!["mint".into()],
                    events: 0,
                },
                AssetChangeJson::Issue {
                    asset: minted,
                    to: bob,
                    amount: 10,
                    events: 0,
                },
            ],
        ),
        (
            to_burner,
            vec![AssetChangeJson::Issue {
                asset: minted,
                to: burner,
                amount: 5,
                events: 0,
            }],
        ),
        (
            burned,
            vec![AssetChangeJson::Burn {
                asset: minted,
                from: burner,
                amount: 2,
                events: 0,
            }],
        ),
    ];
    for (op, changes) in &expected {
        let given = receipt(&rpc, op);
        assert_eq!(given.asset_changes.as_ref(), Some(changes));
        // ksana_getBlock gives the same receipt
        let block = rpc.block(BlockId::Hash(block_of(&rpc, op))).unwrap();
        assert_eq!(block.receipts, [given]);
    }

    // the JSON form: the kind, hex, amounts as decimal strings, the events before it as a number
    let json = serde_json::to_value(receipt(&rpc, sent)).unwrap();
    assert_eq!(
        json["asset_changes"],
        json!([{
            "kind": "transfer",
            "asset": hex::encode(asset),
            "from": hex::encode(alice),
            "to": hex::encode(bob),
            "amount": "30",
            "events": 0,
        }])
    );
    let json = serde_json::to_value(receipt(&rpc, create)).unwrap();
    assert_eq!(
        json["asset_changes"][0],
        json!({ "kind": "create", "asset": hex::encode(asset), "capabilities": ["freeze", "pause"], "events": 0 })
    );
    let json = serde_json::to_value(receipt(&rpc, burned)).unwrap();
    assert_eq!(
        json["asset_changes"],
        json!([{ "kind": "burn", "asset": hex::encode(minted), "from": hex::encode(burner), "amount": "2", "events": 0 }])
    );

    // ksana_call's receipt has the changes of the call: here the funds it attaches
    let called = rpc
        .call(CallRequestJson {
            contract: probe,
            method: "funds".into(),
            args: Vec::new(),
            funds: vec![Funds {
                asset: KSA_ASSET_ID,
                amount: 3,
            }],
            caller: Some(alice),
            max_gas: None,
        })
        .unwrap();
    assert!(called.success, "{:?}", called.error);
    assert_eq!(
        called.asset_changes,
        Some(vec![AssetChangeJson::Funds {
            asset: KSA_ASSET_ID,
            from: alice,
            to: probe,
            amount: 3,
            events: 0,
        }])
    );

    // Bob's listing: the transfers to him by their action (D-111), and the freeze, the payout and the issuance by
    // their changes
    assert_eq!(listed(&rpc, bob), ids(&[to_bob, ksa, paid, freeze, sent]));
    // an asset's listing is its history
    assert_eq!(listed(&rpc, asset), ids(&[freeze, sent, create]));
    assert_eq!(listed(&rpc, minted), ids(&[burned, to_burner, to_bob]));
    // the probe's: the calls of it, but not the deployment that attached funds to it
    assert_eq!(
        listed(&rpc, probe),
        ids(&[burned, to_burner, to_bob, paid, freeze, create])
    );
    // the second probe's: what was issued to it and what it burned, called by the first, but not its deployment
    assert_eq!(listed(&rpc, burner), ids(&[burned, to_burner]));
    // nothing under KSA's ID, which is the zero account
    assert_eq!(listed(&rpc, KSA_ASSET_ID), Vec::<[u8; 32]>::new());
    assert_eq!(listed(&rpc, alice).len(), 10);
}

/// A receipt as a version before D-122 stored it.
#[derive(BorshSerialize)]
struct StoredBefore {
    op_id: Option<[u8; 32]>,
    status: OpStatus,
    gas_used: u64,
    output: Vec<u8>,
    events: Vec<Event>,
}

fn raw_db(chain_dir: &Path) -> rocksdb::DB {
    let names = rocksdb::DB::list_cf(&rocksdb::Options::default(), chain_dir).unwrap();
    rocksdb::DB::open_cf(&rocksdb::Options::default(), chain_dir, names).unwrap()
}

#[test]
fn receipts_stored_before_d122_read_as_not_recorded() {
    let s = scenario("m41-changes/b");
    let rpc = rpc_for(&s.side.chain);
    let (bob, asset) = (s.bob.account, s.asset);
    let freeze = s.ops[3].clone();
    let before = receipt(&rpc, &freeze);
    let block = block_of(&rpc, &freeze);
    let listed_before = (listed(&rpc, bob), listed(&rpc, asset));
    drop(rpc);
    let Side {
        chain,
        producer,
        dir,
        ..
    } = s.side;
    drop(producer);
    drop(chain);
    let chain_dir = dir.path().join("chain");

    // The freeze's block as a version before D-122 stored its receipts: Borsh of them without their changes
    {
        let db = raw_db(&chain_dir);
        let cf = db.cf_handle("receipts").unwrap();
        let value = db.get_cf(cf, block).unwrap().unwrap();
        let stored = value.strip_prefix(RECEIPTS_MAGIC.as_slice()).unwrap();
        assert_eq!(stored[0], 2, "the format byte");
        let receipts: Vec<Receipt> = borsh::from_slice(&stored[1..]).unwrap();
        let old: Vec<StoredBefore> = receipts
            .into_iter()
            .map(|r| StoredBefore {
                op_id: r.op_id,
                status: r.status,
                gas_used: r.gas_used,
                output: r.output,
                events: r.events,
            })
            .collect();
        db.put_cf(cf, block, borsh::to_vec(&old).unwrap()).unwrap();
    }
    let open = || {
        let chain = Arc::new(Chain::open(dir.path(), DEVNET, &s.genesis, s.mock.clone()).unwrap());
        assert!(chain.store().index_current().unwrap());
        rpc_for(&chain)
    };
    let rpc = open();
    let now = receipt(&rpc, &freeze);
    assert_eq!(now.asset_changes, None);
    assert_eq!(
        now,
        ReceiptJson {
            asset_changes: None,
            ..before
        }
    );
    assert_eq!(
        serde_json::to_value(&now).unwrap()["asset_changes"],
        serde_json::Value::Null
    );
    let shown = rpc.block(BlockId::Hash(block)).unwrap();
    assert_eq!(shown.receipts, [now]);
    // the other blocks' receipts are untouched, and the index kept its listings
    assert!(receipt(&rpc, &s.ops[2]).asset_changes.is_some());
    assert_eq!((listed(&rpc, bob), listed(&rpc, asset)), listed_before);
    drop(rpc);

    // An index built again from those receipts lists the freeze by its action alone, as that version did
    {
        let db = raw_db(&chain_dir);
        let meta = db.cf_handle("meta").unwrap();
        db.delete_cf(meta, b"index").unwrap();
    }
    let rpc = open();
    let without = |listing: Vec<[u8; 32]>| -> Vec<[u8; 32]> {
        let freeze = freeze.op_id().unwrap();
        listing.into_iter().filter(|id| *id != freeze).collect()
    };
    assert_eq!(listed(&rpc, bob), without(listed_before.0));
    assert_eq!(listed(&rpc, asset), without(listed_before.1));
}
