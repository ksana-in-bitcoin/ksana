//! The rules of an asset with issuer capabilities (D-121) hold on every path that moves it: the `AssetTransfer` action,
//! the funds attached to a `Call`, a `Deploy` or the RPC's read-only `ksana_call`, `asset_transfer`, the funds of
//! `call_contract`, `try_call_contract` and `create_contract`, `asset_issue` and `asset_burn`. A refusal fails the op,
//! or only the level of a `try_call_contract`, never the block, and its message starts with `rejected:` on every path.
//! The issuing contract is the test contract `probe`, whose methods anyone may call.

mod common;

use std::sync::Arc;

use common::probe::*;
use common::*;
use ksana_execution::{NoBtc, OpStatus, Receipt};
use ksana_primitives::asset::{
    CAP_ALLOWLIST, CAP_FREEZE, CAP_MINT, CAP_PAUSE, STATUS_ALLOWED, STATUS_FROZEN,
};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Funds};
use ksana_test_contracts::PROBE;
use ksana_vm::AssetMetaInput;

fn meta() -> AssetMetaInput {
    AssetMetaInput {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
    }
}

fn ok(r: &Receipt) -> &[u8] {
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    &r.output
}

/// Alice, Bob, the issuing contract, a second contract holding the asset, and the asset.
struct Setup {
    w: World,
    alice: User,
    bob: User,
    issuer: [u8; 32],
    holder: [u8; 32],
    asset: [u8; 32],
}

/// `issuer` creates its asset under `sub_id` with `capabilities`, keeps 700 and sends 200 to Alice and 100 to `holder`
/// (both allowed first when the asset has an allowlist).
fn setup(sub_id: [u8; 32], capabilities: u8) -> Setup {
    let alice = user("alice");
    let bob = user("bob");
    let mut w = World::new(&[&alice, &bob]);
    let issuer = w.deploy(&alice, vec![]);
    let holder = w.deploy(&alice, vec![]);
    let args = enc(&(sub_id, meta(), capabilities, issuer, 1_000u128));
    let r = w.run(&alice, call(issuer, "create_asset", args, vec![]));
    let asset: [u8; 32] = borsh::from_slice(ok(&r)).unwrap();
    let mut s = Setup {
        w,
        alice,
        bob,
        issuer,
        holder,
        asset,
    };
    for (to, amount) in [(s.alice.account, 200u128), (holder, 100)] {
        if capabilities & CAP_ALLOWLIST != 0 {
            s.manage("allow", enc(&(sub_id, to, true)));
        }
        let r = s.issuer_sends(&to, amount);
        ok(&r);
    }
    s
}

impl Setup {
    /// Calls an asset management method of the issuing contract, which must succeed.
    fn manage(&mut self, method: &str, args: Vec<u8>) {
        let r = self
            .w
            .run(&self.bob, call(self.issuer, method, args, vec![]));
        ok(&r);
    }

    fn issuer_sends(&mut self, to: &[u8; 32], amount: u128) -> Receipt {
        let args = enc(&(self.asset, *to, amount));
        self.w
            .run(&self.bob, call(self.issuer, "send", args, vec![]))
    }

    fn holder_sends(&mut self, to: &[u8; 32], amount: u128) -> Receipt {
        let args = enc(&(self.asset, *to, amount));
        self.w
            .run(&self.bob, call(self.holder, "send", args, vec![]))
    }

    fn alice_sends(&mut self, to: &[u8; 32], amount: u128) -> Receipt {
        let action = Action::AssetTransfer {
            asset: self.asset,
            to: *to,
            amount,
        };
        self.w.run(&self.alice, action)
    }

    fn funds(&self, amount: u128) -> Vec<Funds> {
        vec![Funds {
            asset: self.asset,
            amount,
        }]
    }

    fn balance(&self, account: &[u8; 32]) -> u128 {
        self.w.balance(&self.asset, account)
    }
}

#[test]
fn a_frozen_account_is_refused_on_every_path() {
    let sub_id = [3u8; 32];
    let mut s = setup(sub_id, CAP_FREEZE | CAP_PAUSE);
    let alice = s.alice.account;
    let bob = s.bob.account;
    s.manage("freeze", enc(&(sub_id, alice, true)));
    let r = s.w.run(
        &s.bob,
        call(s.issuer, "status", enc(&(s.asset, alice)), vec![]),
    );
    assert_eq!(ok(&r), [STATUS_FROZEN]);

    // the AssetTransfer action and the funds attached to a call or a deployment: the op fails, Alice still pays its fee
    let before = s.w.chain.ksa(&alice);
    let r = s.alice_sends(&bob, 1);
    assert_eq!(failed(&r), "rejected: sender is frozen for this asset");
    let funds = s.funds(1);
    let r =
        s.w.run(&s.alice, call(s.holder, "noop", vec![], funds.clone()));
    assert_eq!(failed(&r), "rejected: sender is frozen for this asset");
    let r = s.w.run(&s.alice, deploy_probe(b"ok", funds.clone()));
    assert_eq!(failed(&r), "rejected: sender is frozen for this asset");
    assert_eq!(s.w.chain.ksa(&alice), before - 3 * FEE);
    // the read-only call of the RPC's ksana_call, which a wallet runs before sending the call
    let chain = &s.w.chain;
    let r = chain
        .exec
        .simulate_call(
            Arc::new(chain.state.clone()),
            Arc::new(NoBtc),
            &chain.env(),
            alice,
            s.holder,
            "noop".into(),
            vec![],
            funds,
            GAS,
        )
        .unwrap();
    assert_eq!(failed(&r), "rejected: sender is frozen for this asset");
    // a contract's asset_transfer to her
    let r = s.holder_sends(&alice, 1);
    assert_eq!(failed(&r), "rejected: recipient is frozen for this asset");

    // a frozen contract: asset_transfer, the funds of call_contract, try_call_contract and create_contract, asset_burn
    let holder = s.holder;
    s.manage("freeze", enc(&(sub_id, holder, true)));
    let r = s.holder_sends(&bob, 1);
    assert_eq!(failed(&r), "rejected: sender is frozen for this asset");
    let forward_args = forward(s.issuer, "noop", vec![], s.funds(1));
    let r =
        s.w.run(&s.bob, call(holder, "forward", forward_args, vec![]));
    assert_eq!(failed(&r), "rejected: sender is frozen for this asset");
    let try_args = enc(&(
        s.issuer,
        "noop".to_string(),
        Vec::<u8>::new(),
        s.funds(1),
        0u64,
    ));
    let r =
        s.w.run(&s.bob, call(holder, "try_forward", try_args, vec![]));
    let (status, data): (u8, Vec<u8>) = borsh::from_slice(ok(&r)).unwrap();
    assert_eq!(
        (status, data),
        (2, vec![]),
        "a caught failure of that level"
    );
    let create_args = enc(&(sha256(PROBE), [5u8; 32], enc(&b"ok".to_vec()), s.funds(1)));
    let r = s.w.run(&s.bob, call(holder, "create", create_args, vec![]));
    assert_eq!(failed(&r), "rejected: sender is frozen for this asset");
    let r =
        s.w.run(&s.bob, call(holder, "burn", enc(&(s.asset, 1u128)), vec![]));
    assert_eq!(failed(&r), "rejected: sender is frozen for this asset");
    assert_eq!(
        (s.balance(&alice), s.balance(&holder), s.balance(&bob)),
        (200, 100, 0)
    );

    // unfrozen, both move again
    s.manage("freeze", enc(&(sub_id, alice, false)));
    s.manage("freeze", enc(&(sub_id, holder, false)));
    ok(&s.alice_sends(&bob, 20));
    ok(&s.holder_sends(&bob, 10));
    assert_eq!(s.balance(&bob), 30);
}

#[test]
fn a_pause_stops_everyone_but_the_issuing_contract() {
    let sub_id = [3u8; 32];
    let mut s = setup(sub_id, CAP_MINT | CAP_PAUSE);
    let bob = s.bob.account;
    s.manage("pause", enc(&(sub_id, true)));
    let r =
        s.w.run(&s.bob, call(s.issuer, "policy", enc(&s.asset), vec![]));
    assert_eq!(
        borsh::from_slice::<Option<(u8, bool)>>(ok(&r)).unwrap(),
        Some((CAP_MINT | CAP_PAUSE, true))
    );
    assert_eq!(failed(&s.alice_sends(&bob, 1)), "rejected: asset is paused");
    assert_eq!(
        failed(&s.holder_sends(&bob, 1)),
        "rejected: asset is paused"
    );
    // the issuing contract still sends and issues
    ok(&s.issuer_sends(&bob, 5));
    let args = enc(&(sub_id, meta(), bob, 7u128));
    let r = s.w.run(&s.bob, call(s.issuer, "issue_meta", args, vec![]));
    ok(&r);
    // giving up the pause resumes the asset for good
    s.manage("renounce", enc(&(sub_id, CAP_PAUSE)));
    ok(&s.alice_sends(&bob, 1));
    let r = s.w.run(
        &s.bob,
        call(s.issuer, "pause", enc(&(sub_id, true)), vec![]),
    );
    assert_eq!(failed(&r), "asset does not have the pause capability");
    assert_eq!(s.balance(&bob), 13);
}

#[test]
fn an_allowlist_and_a_fixed_supply_hold_on_chain() {
    let sub_id = [4u8; 32];
    let mut s = setup(sub_id, CAP_ALLOWLIST);
    let alice = s.alice.account;
    let bob = s.bob.account;
    assert_eq!(
        failed(&s.alice_sends(&bob, 1)),
        "rejected: recipient is not on this asset's allowlist"
    );
    s.manage("allow", enc(&(sub_id, bob, true)));
    ok(&s.alice_sends(&bob, 50));
    let r = s.w.run(
        &s.bob,
        call(s.issuer, "status", enc(&(s.asset, bob)), vec![]),
    );
    assert_eq!(ok(&r), [STATUS_ALLOWED]);
    s.manage("allow", enc(&(sub_id, alice, false)));
    assert_eq!(
        failed(&s.alice_sends(&bob, 1)),
        "rejected: sender is not on this asset's allowlist"
    );
    // created without minting: the supply stays at 1 000
    let args = enc(&(sub_id, meta(), bob, 1u128));
    let r = s.w.run(&s.bob, call(s.issuer, "issue_meta", args, vec![]));
    assert_eq!(failed(&r), "rejected: asset supply is fixed");
    // only the issuing contract manages its asset: another contract's call names its own asset, which does not exist
    let holder = s.holder;
    let r = s.w.run(
        &s.bob,
        call(holder, "allow", enc(&(sub_id, alice, true)), vec![]),
    );
    assert_eq!(failed(&r), "asset does not exist");
    let meta_of: ksana_execution::keys::AssetMeta =
        s.w.chain
            .get(&ksana_execution::keys::asset(&s.asset))
            .unwrap();
    assert_eq!(
        (meta_of.total_supply, meta_of.capabilities, meta_of.paused),
        (1_000, CAP_ALLOWLIST, false)
    );
    assert_eq!(
        (s.balance(&alice), s.balance(&bob), s.balance(&holder)),
        (150, 50, 100)
    );
}
