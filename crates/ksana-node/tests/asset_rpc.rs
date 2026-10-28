//! `ksana_getAsset` reports an asset's symbol, the capabilities its issuing contract holds and its pause, and
//! `ksana_getAssetStatus` an account's standing in it (D-121), read from the node's main chain. The issuing contract is
//! the test contract `probe`, whose methods anyone may call.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;

use common::side::{setup, Side};
use common::{user, User};
use ksana_crypto::sign_vmop;
use ksana_execution::builtin::{asset_id, contract_id};
use ksana_node::producer::FeePolicy;
use ksana_node::rpc::NodeRpc;
use ksana_primitives::asset::{CAP_FREEZE, CAP_MINT, CAP_PAUSE};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, FeeMode, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION};
use ksana_rpc::{codes, AssetStatusJson, Backend};
use ksana_test_contracts::PROBE;

fn op(u: &User, nonce: u64, action: Action) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::Token { amount: 100 },
        max_gas: 2_000_000,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[2; 32]).unwrap();
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

#[test]
fn the_rpc_reports_capabilities_the_pause_and_an_account_s_standing() {
    let (alice, bob) = (user("asset-rpc/alice"), user("asset-rpc/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let node = Side::new(&mock, &genesis, "asset-rpc");
    let rpc = NodeRpc {
        chain: Arc::clone(&node.chain),
        mempool: Arc::clone(&node.mempool),
        sync: None,
        paused: Arc::new(AtomicBool::new(false)),
        events: node.producer.events.clone(),
        relay: None,
        fees: FeePolicy::new(1),
        calls: AtomicUsize::new(0),
    };

    // KSA: a symbol, no capabilities, everyone may send and receive it
    let ksa = rpc.asset(KSA_ASSET_ID).unwrap();
    assert_eq!(
        (ksa.name.as_str(), ksa.symbol.as_str(), ksa.decimals),
        ("KSA", "KSA", 8)
    );
    assert!(ksa.capabilities.is_empty() && !ksa.paused);
    let standing = rpc.asset_status(KSA_ASSET_ID, alice.account).unwrap();
    assert!(standing.status.is_empty() && standing.can_send && standing.can_receive);
    let missing = rpc.asset_status([7; 32], alice.account).unwrap_err();
    assert_eq!(missing.code(), codes::NOT_FOUND, "{missing}");

    // the probe creates an asset with minting, freezing and a pause, and gives Alice 100 of it
    let probe = contract_id(&alice.account, 0, &sha256(PROBE));
    let sub_id = [6u8; 32];
    let asset = asset_id(&probe, &sub_id);
    let meta = ("Dollar".to_string(), "USDX".to_string(), 6u8);
    let capabilities = CAP_MINT | CAP_FREEZE | CAP_PAUSE;
    let steps = [
        Action::Deploy {
            code: PROBE.to_vec(),
            init_args: borsh::to_vec(&b"ok".to_vec()).unwrap(),
            funds: Vec::new(),
        },
        call(
            probe,
            "create_asset",
            &(sub_id, meta, capabilities, alice.account, 100u128),
        ),
        call(probe, "freeze", &(sub_id, alice.account, true)),
        call(probe, "pause", &(sub_id, true)),
    ];
    for (nonce, action) in (0u64..).zip(steps) {
        node.submit(op(&alice, nonce, action));
        assert_eq!(node.mine().entries.len(), 1);
    }

    let info = rpc.asset(asset).unwrap();
    assert_eq!(
        (
            info.symbol.as_str(),
            info.total_supply,
            info.issuer,
            info.paused
        ),
        ("USDX", 100, probe, true)
    );
    assert_eq!(info.capabilities, ["mint", "freeze", "pause"]);
    let standing = |account: [u8; 32]| -> AssetStatusJson {
        let s = rpc.asset_status(asset, account).unwrap();
        assert_eq!((s.asset, s.account), (asset, account));
        s
    };
    let a = standing(alice.account);
    assert_eq!(a.status, ["frozen"]);
    assert!(!a.can_send && !a.can_receive);
    // Bob is not frozen but the pause keeps him from sending; the issuing contract still moves it
    let b = standing(bob.account);
    assert!(b.status.is_empty() && !b.can_send && b.can_receive);
    let p = standing(probe);
    assert!(p.status.is_empty() && p.can_send && p.can_receive);
    assert_eq!(a.at, info.at);
}
