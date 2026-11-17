//! M67: the local holdings index keeps account balance RPCs proportional to the assets an account has held (D-155).

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use common::side::{setup, Side};
use common::{user, User};
use ksana_crypto::sign_vmop;
use ksana_execution::builtin::{asset_id, contract_id};
use ksana_execution::keys;
use ksana_mempool::{Mempool, Policy};
use ksana_node::chain::{Accepted, Chain};
use ksana_node::producer::FeePolicy;
use ksana_node::rpc::NodeRpc;
use ksana_primitives::asset::CAP_MINT;
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, FeeMode, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION};
use ksana_rpc::{AccountAssetJson, Backend};
use ksana_test_contracts::PROBE;
use tokio::sync::broadcast;

fn op(u: &User, nonce: u64, action: Action) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::Token { amount: 100 },
        max_gas: 3_000_000,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[5; 32]).unwrap();
    op
}

fn call<T: borsh::BorshSerialize>(contract: [u8; 32], method: &str, args: &T) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args: borsh::to_vec(args).unwrap(),
        funds: Vec::new(),
    }
}

fn rpc(chain: &Arc<Chain>) -> NodeRpc {
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

fn oracle(chain: &Chain, account: [u8; 32]) -> Vec<([u8; 32], u128)> {
    chain
        .with_state(|state, _| {
            let mut balances = state
                .scan_prefix(&[keys::PREFIX_BALANCE])
                .unwrap()
                .into_iter()
                .filter_map(|(key, value)| {
                    if key.len() != 65 || key[33..] != account {
                        return None;
                    }
                    let asset = key[1..33].try_into().ok()?;
                    let amount = borsh::from_slice(&value).unwrap();
                    Some((asset, amount))
                })
                .collect::<Vec<_>>();
            balances.sort_by_key(|(asset, _)| *asset);
            balances
        })
        .unwrap()
}

fn assert_account_matches_oracle(rpc: &NodeRpc, chain: &Chain, account: [u8; 32]) {
    let expected = oracle(chain, account);
    let account_json = rpc.account(account).unwrap();
    let actual = account_json
        .balances
        .iter()
        .map(|balance| (balance.asset, balance.amount))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);

    let expected_assets = expected
        .iter()
        .map(|(asset, balance)| AccountAssetJson::Asset {
            asset: *asset,
            balance: *balance,
        })
        .collect::<Vec<_>>();
    let mut actual_assets = Vec::new();
    let mut after = None;
    loop {
        let page = rpc.account_assets(account, after, 1).unwrap();
        actual_assets.extend(
            page.assets
                .into_iter()
                .filter(|asset| matches!(asset, AccountAssetJson::Asset { .. })),
        );
        after = page.next.map(|next| hex::decode(next).unwrap());
        if after.is_none() {
            break;
        }
    }
    assert_eq!(actual_assets, expected_assets);
}

fn raw_db(chain_dir: &Path) -> rocksdb::DB {
    let names = rocksdb::DB::list_cf(&rocksdb::Options::default(), chain_dir).unwrap();
    rocksdb::DB::open_cf(&rocksdb::Options::default(), chain_dir, names).unwrap()
}

#[test]
fn holdings_cover_rpc_mutations_forks_rebuild_and_account_scaling() {
    let (alice, bob, carol) = (user("m67/alice"), user("m67/bob"), user("m67/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000), (&bob, 10_000), (&carol, 10_000)]);
    let a = Side::new(&mock, &genesis, "m67/a");
    let b = Side::new(&mock, &genesis, "m67/b");
    let a_rpc = rpc(&a.chain);

    assert_account_matches_oracle(&a_rpc, &a.chain, alice.account);

    let probe = contract_id(&alice.account, 0, &sha256(PROBE));
    let sub_id = [6; 32];
    let asset = asset_id(&probe, &sub_id);
    let initial = [
        Action::Deploy {
            code: PROBE.to_vec(),
            init_args: borsh::to_vec(&b"ok".to_vec()).unwrap(),
            funds: Vec::new(),
        },
        call(
            probe,
            "create_asset",
            &(
                sub_id,
                ("Dollar".to_string(), "USDX".to_string(), 6u8),
                CAP_MINT,
                bob.account,
                100u128,
            ),
        ),
    ];
    for (nonce, action) in (0u64..).zip(initial) {
        a.submit(op(&alice, nonce, action));
        let block = a.mine();
        b.accept(&block).unwrap();
    }
    assert_account_matches_oracle(&a_rpc, &a.chain, bob.account);

    for (nonce, amount) in [(0, 40), (1, 60)] {
        a.submit(op(
            &bob,
            nonce,
            Action::AssetTransfer {
                asset,
                to: carol.account,
                amount,
            },
        ));
        let block = a.mine();
        b.accept(&block).unwrap();
        assert_account_matches_oracle(&a_rpc, &a.chain, bob.account);
    }
    assert!(!oracle(&a.chain, bob.account)
        .iter()
        .any(|(held_asset, _)| *held_asset == asset));

    // A empties Carol's token balance and credits Bob; the longer B branch keeps part of Carol's balance instead.
    a.submit(op(
        &carol,
        0,
        Action::AssetTransfer {
            asset,
            to: bob.account,
            amount: 100,
        },
    ));
    let a_blocks = [a.mine(), a.mine()];
    assert!(!oracle(&a.chain, carol.account)
        .iter()
        .any(|(held_asset, _)| *held_asset == asset));
    assert!(oracle(&a.chain, bob.account)
        .iter()
        .any(|(held_asset, _)| *held_asset == asset));
    b.submit(op(
        &carol,
        0,
        Action::AssetTransfer {
            asset,
            to: alice.account,
            amount: 30,
        },
    ));
    let b_blocks: Vec<_> = (0..4).map(|_| b.mine()).collect();
    let mut reorganized = false;
    for block in &b_blocks {
        let report = a.accept(block).unwrap();
        if matches!(report.outcome, Accepted::Reorganized(_)) {
            reorganized = true;
        }
    }
    assert!(
        reorganized,
        "the longer competing branch should become main"
    );
    assert_eq!(a.chain.tip().unwrap().hash, b_blocks[3].header.hash());
    assert_account_matches_oracle(&a_rpc, &a.chain, carol.account);
    assert_account_matches_oracle(&a_rpc, &a.chain, bob.account);
    assert!(oracle(&a.chain, carol.account)
        .iter()
        .any(|(held_asset, amount)| *held_asset == asset && *amount == 70));
    let bob_assets = a.chain.store().holdings(&bob.account, None).unwrap();
    assert_eq!(bob_assets, [KSA_ASSET_ID, asset]);
    assert!(!oracle(&a.chain, bob.account)
        .iter()
        .any(|(held_asset, _)| *held_asset == asset));
    assert!(a_blocks[0].header.height < b_blocks[3].header.height);

    // More than fifty assets unrelated to Bob do not enter Bob's account-indexed holdings.
    for index in 0..51u8 {
        let sub_id = [index.saturating_add(100); 32];
        a.submit(op(
            &alice,
            u64::from(index) + 2,
            call(
                probe,
                "create_asset",
                &(
                    sub_id,
                    (format!("Asset {index}"), format!("A{index}"), 0u8),
                    CAP_MINT,
                    carol.account,
                    1u128,
                ),
            ),
        ));
        a.mine();
    }
    assert_account_matches_oracle(&a_rpc, &a.chain, bob.account);
    assert_eq!(
        a.chain.store().holdings(&bob.account, None).unwrap(),
        [KSA_ASSET_ID, asset]
    );
    assert_eq!(
        a.chain.store().holdings_base().unwrap(),
        Some(0),
        "the index was built from genesis"
    );

    drop(a_rpc);
    let Side {
        chain,
        producer,
        dir,
        ..
    } = a;
    drop(producer);
    drop(chain);
    let chain_dir = dir.path().join("chain");
    {
        let db = raw_db(&chain_dir);
        let mut batch = rocksdb::WriteBatch::default();
        let holdings = db.cf_handle("holdings").unwrap();
        for item in db.iterator_cf(holdings, rocksdb::IteratorMode::Start) {
            let (key, _) = item.unwrap();
            batch.delete_cf(holdings, key);
        }
        let meta = db.cf_handle("meta").unwrap();
        batch.delete_cf(meta, b"holdings");
        db.write(batch).unwrap();
        assert!(db.get_cf(meta, b"holdings").unwrap().is_none());
        assert!(db
            .iterator_cf(holdings, rocksdb::IteratorMode::Start)
            .next()
            .is_none());
    }
    let reopened = Arc::new(Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap());
    assert_eq!(
        reopened.store().holdings_base().unwrap(),
        Some(reopened.tip().unwrap().header.height)
    );
    let reopened_rpc = rpc(&reopened);
    assert_account_matches_oracle(&reopened_rpc, &reopened, carol.account);
}
