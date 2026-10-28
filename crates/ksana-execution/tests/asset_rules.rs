//! The issuer capabilities of built-in assets as the Asset module applies them (D-121): the metadata limits, what each
//! capability lets the issuing contract do, which movements an asset's rules refuse, and what renouncing leaves.

use std::sync::Arc;

use ksana_execution::builtin::{self, Bar, Fault};
use ksana_execution::keys::{self, AssetMeta};
use ksana_execution::{MemState, Overlay};
use ksana_primitives::asset::{
    CAPABILITIES, CAP_ALLOWLIST, CAP_FREEZE, CAP_MINT, CAP_PAUSE, STATUS_ALLOWED, STATUS_FROZEN,
};
use ksana_vm::AssetMetaInput;

const ISSUER: [u8; 32] = [1; 32];
const ALICE: [u8; 32] = [0xA1; 32];
const BOB: [u8; 32] = [0xB0; 32];

fn meta() -> AssetMetaInput {
    AssetMetaInput {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
    }
}

fn asset() -> [u8; 32] {
    builtin::asset_id(&ISSUER, &[9; 32])
}

fn empty() -> Overlay {
    Overlay::new(Arc::new(MemState::default()))
}

/// The asset created with `capabilities` and 1 000 issued to its issuing contract, which sends 100 to ALICE (allowed
/// first when the asset has an allowlist).
fn setup(capabilities: u8) -> Overlay {
    let mut ov = empty();
    builtin::create(
        &mut ov,
        &asset(),
        &ISSUER,
        &meta(),
        capabilities,
        &ISSUER,
        1_000,
    )
    .unwrap();
    if capabilities & CAP_ALLOWLIST != 0 {
        allow(&mut ov, &ALICE, true).unwrap();
    }
    builtin::transfer(&mut ov, &asset(), &ISSUER, &ALICE, 100).unwrap();
    ov
}

fn recorded(ov: &Overlay) -> AssetMeta {
    builtin::asset_meta(ov, &asset()).unwrap().unwrap()
}

fn balance(ov: &Overlay, account: &[u8; 32]) -> u128 {
    builtin::balance(ov, &asset(), account).unwrap()
}

fn send(ov: &mut Overlay, from: &[u8; 32], to: &[u8; 32], amount: u128) -> Result<(), Fault> {
    builtin::transfer(ov, &asset(), from, to, amount)
}

fn freeze(ov: &mut Overlay, account: &[u8; 32], on: bool) -> Result<(), Fault> {
    builtin::set_status(ov, &asset(), &ISSUER, account, STATUS_FROZEN, on)
}

fn allow(ov: &mut Overlay, account: &[u8; 32], on: bool) -> Result<(), Fault> {
    builtin::set_status(ov, &asset(), &ISSUER, account, STATUS_ALLOWED, on)
}

fn failed(r: Result<(), Fault>) -> String {
    match r {
        Err(Fault::Failed(msg)) => msg,
        other => panic!("should fail the op, got {other:?}"),
    }
}

#[test]
fn create_records_the_declared_capabilities_and_checks_the_metadata() {
    for capabilities in 0..=CAPABILITIES {
        let ov = setup(capabilities);
        assert_eq!(
            recorded(&ov),
            AssetMeta {
                name: "Dollar".into(),
                symbol: "USDX".into(),
                decimals: 6,
                total_supply: 1_000,
                issuer: ISSUER,
                capabilities,
                paused: false,
            }
        );
        assert_eq!((balance(&ov, &ISSUER), balance(&ov, &ALICE)), (900, 100));
    }
    let create = |ov: &mut Overlay, meta: &AssetMetaInput, capabilities: u8| {
        builtin::create(ov, &asset(), &ISSUER, meta, capabilities, &ISSUER, 1)
    };
    for capabilities in [0x10, 0x80, 0xFF] {
        assert_eq!(
            failed(create(&mut empty(), &meta(), capabilities)),
            "undefined capability bits"
        );
    }
    let mut ov = setup(0);
    assert_eq!(failed(create(&mut ov, &meta(), 0)), "asset already exists");
    let with = |name: &str, symbol: &str, decimals: u8| AssetMetaInput {
        name: name.into(),
        symbol: symbol.into(),
        decimals,
    };
    // the limits count bytes: "₿" is three
    for (bad, expected) in [
        (with("", "USDX", 6), "asset name must be 1 to 64 bytes"),
        (
            with(&"a".repeat(65), "USDX", 6),
            "asset name must be 1 to 64 bytes",
        ),
        (
            with(&"₿".repeat(22), "USDX", 6),
            "asset name must be 1 to 64 bytes",
        ),
        (with("Dollar", "", 6), "asset symbol must be 1 to 16 bytes"),
        (
            with("Dollar", &"S".repeat(17), 6),
            "asset symbol must be 1 to 16 bytes",
        ),
        (
            with("Dollar", &"₿".repeat(6), 6),
            "asset symbol must be 1 to 16 bytes",
        ),
        (
            with("Dollar", "USDX", 19),
            "asset decimals must be at most 18",
        ),
    ] {
        assert_eq!(failed(create(&mut empty(), &bad, 0)), expected, "{bad:?}");
    }
    for good in [
        with(&"a".repeat(64), "USDX", 6),
        with(&"₿".repeat(21), &"₿".repeat(5), 18),
        with("ñ", "ü", 0),
    ] {
        create(&mut empty(), &good, 0).unwrap();
    }
}

#[test]
fn issuance_needs_mint() {
    // asset_issue creates an asset with minting alone, and keeps issuing while it has it
    let mut ov = empty();
    builtin::issue(&mut ov, &asset(), &ISSUER, &meta(), &ALICE, 10).unwrap();
    assert_eq!(recorded(&ov).capabilities, CAP_MINT);
    builtin::issue(&mut ov, &asset(), &ISSUER, &meta(), &BOB, 5).unwrap();
    assert_eq!(recorded(&ov).total_supply, 15);
    let other = AssetMetaInput {
        symbol: "USDY".into(),
        ..meta()
    };
    assert_eq!(
        failed(builtin::issue(&mut ov, &asset(), &ISSUER, &other, &BOB, 5)),
        "asset metadata does not match the recorded one"
    );
    // nor does another issuer's: the VM derives an asset's ID from its issuing contract, so it never names one
    assert_eq!(
        failed(builtin::issue(&mut ov, &asset(), &ALICE, &meta(), &BOB, 5)),
        "asset metadata does not match the recorded one"
    );
    assert_eq!((recorded(&ov).total_supply, balance(&ov, &BOB)), (15, 5));
    // without minting the supply is fixed for good
    builtin::renounce(&mut ov, &asset(), &ISSUER, CAP_MINT).unwrap();
    assert_eq!(
        failed(builtin::issue(&mut ov, &asset(), &ISSUER, &meta(), &BOB, 5)),
        "rejected: asset supply is fixed"
    );
    for capabilities in [0, CAP_FREEZE | CAP_ALLOWLIST | CAP_PAUSE] {
        let mut ov = setup(capabilities);
        assert_eq!(
            failed(builtin::issue(
                &mut ov,
                &asset(),
                &ISSUER,
                &meta(),
                &ISSUER,
                1
            )),
            "rejected: asset supply is fixed"
        );
    }
    let mut ov = setup(CAP_MINT);
    builtin::issue(&mut ov, &asset(), &ISSUER, &meta(), &BOB, 7).unwrap();
    assert_eq!((recorded(&ov).total_supply, balance(&ov, &BOB)), (1_007, 7));
}

#[test]
fn a_frozen_account_neither_sends_receives_nor_burns() {
    let mut ov = setup(CAP_MINT | CAP_FREEZE);
    freeze(&mut ov, &ALICE, true).unwrap();
    // whatever the amount
    for amount in [0, 1] {
        assert_eq!(
            failed(send(&mut ov, &ALICE, &BOB, amount)),
            "rejected: sender is frozen for this asset"
        );
        assert_eq!(
            failed(send(&mut ov, &ISSUER, &ALICE, amount)),
            "rejected: recipient is frozen for this asset"
        );
        assert_eq!(
            failed(builtin::burn(&mut ov, &asset(), &ALICE, amount)),
            "rejected: sender is frozen for this asset"
        );
        assert_eq!(
            failed(builtin::issue(
                &mut ov,
                &asset(),
                &ISSUER,
                &meta(),
                &ALICE,
                amount
            )),
            "rejected: recipient is frozen for this asset"
        );
    }
    // the others go on
    send(&mut ov, &ISSUER, &BOB, 10).unwrap();
    send(&mut ov, &BOB, &ISSUER, 1).unwrap();
    freeze(&mut ov, &ALICE, false).unwrap();
    send(&mut ov, &ALICE, &BOB, 40).unwrap();
    assert_eq!((balance(&ov, &ALICE), balance(&ov, &BOB)), (60, 49));
}

#[test]
fn an_allowlist_admits_only_the_allowed_accounts() {
    let mut ov = setup(CAP_ALLOWLIST);
    assert_eq!(
        failed(send(&mut ov, &ALICE, &BOB, 1)),
        "rejected: recipient is not on this asset's allowlist"
    );
    allow(&mut ov, &BOB, true).unwrap();
    send(&mut ov, &ALICE, &BOB, 30).unwrap();
    allow(&mut ov, &ALICE, false).unwrap();
    assert_eq!(
        failed(send(&mut ov, &ALICE, &BOB, 1)),
        "rejected: sender is not on this asset's allowlist"
    );
    assert_eq!(
        failed(builtin::burn(&mut ov, &asset(), &ALICE, 1)),
        "rejected: sender is not on this asset's allowlist"
    );
    // the issuing contract needs no place on the list
    send(&mut ov, &BOB, &ISSUER, 5).unwrap();
    send(&mut ov, &ISSUER, &BOB, 50).unwrap();
    builtin::burn(&mut ov, &asset(), &ISSUER, 5).unwrap();
    assert_eq!(
        (
            balance(&ov, &ALICE),
            balance(&ov, &BOB),
            recorded(&ov).total_supply
        ),
        (70, 75, 995)
    );
    // a creation with an allowlist can only go to the issuing contract
    let mut ov = empty();
    assert_eq!(
        failed(builtin::create(
            &mut ov,
            &asset(),
            &ISSUER,
            &meta(),
            CAP_ALLOWLIST,
            &ALICE,
            1
        )),
        "rejected: recipient is not on this asset's allowlist"
    );
}

#[test]
fn a_pause_stops_every_movement_but_the_issuer_s() {
    let mut ov = setup(CAP_MINT | CAP_PAUSE);
    builtin::set_paused(&mut ov, &asset(), &ISSUER, true).unwrap();
    assert!(recorded(&ov).paused);
    for amount in [0, 1] {
        assert_eq!(
            failed(send(&mut ov, &ALICE, &BOB, amount)),
            "rejected: asset is paused"
        );
        assert_eq!(
            failed(builtin::burn(&mut ov, &asset(), &ALICE, amount)),
            "rejected: asset is paused"
        );
    }
    // the issuing contract still sends, issues and burns
    send(&mut ov, &ISSUER, &BOB, 10).unwrap();
    builtin::issue(&mut ov, &asset(), &ISSUER, &meta(), &BOB, 5).unwrap();
    builtin::burn(&mut ov, &asset(), &ISSUER, 100).unwrap();
    builtin::set_paused(&mut ov, &asset(), &ISSUER, false).unwrap();
    send(&mut ov, &BOB, &ALICE, 15).unwrap();
    assert_eq!(
        (
            balance(&ov, &ALICE),
            balance(&ov, &BOB),
            recorded(&ov).total_supply
        ),
        (115, 0, 905)
    );
}

#[test]
fn setting_needs_the_capability_and_clearing_never_does() {
    let mut ov = setup(0);
    assert_eq!(
        failed(freeze(&mut ov, &ALICE, true)),
        "asset does not have the freeze capability"
    );
    assert_eq!(
        failed(allow(&mut ov, &ALICE, true)),
        "asset does not have the allowlist capability"
    );
    assert_eq!(
        failed(builtin::set_paused(&mut ov, &asset(), &ISSUER, true)),
        "asset does not have the pause capability"
    );
    freeze(&mut ov, &ALICE, false).unwrap();
    allow(&mut ov, &ALICE, false).unwrap();
    builtin::set_paused(&mut ov, &asset(), &ISSUER, false).unwrap();
    // the issuing contract has no status, only it manages the asset, and only an existing asset
    let mut ov = setup(CAPABILITIES);
    assert_eq!(
        failed(freeze(&mut ov, &ISSUER, true)),
        "the issuing contract has no status for its own asset"
    );
    assert_eq!(
        failed(builtin::set_status(
            &mut ov,
            &asset(),
            &BOB,
            &ALICE,
            STATUS_FROZEN,
            true
        )),
        "only the issuing contract manages an asset"
    );
    assert_eq!(
        failed(builtin::set_paused(&mut ov, &asset(), &BOB, true)),
        "only the issuing contract manages an asset"
    );
    assert_eq!(
        failed(builtin::renounce(&mut ov, &asset(), &BOB, CAP_MINT)),
        "only the issuing contract manages an asset"
    );
    let unknown = builtin::asset_id(&ISSUER, &[8; 32]);
    assert_eq!(
        failed(builtin::set_paused(&mut ov, &unknown, &ISSUER, false)),
        "asset does not exist"
    );
    assert_eq!(
        failed(builtin::set_status(
            &mut ov,
            &asset(),
            &ISSUER,
            &ALICE,
            4,
            true
        )),
        "undefined status flag"
    );
}

#[test]
fn renouncing_only_removes_and_a_flag_without_its_capability_does_not_count() {
    let mut ov = setup(CAPABILITIES);
    allow(&mut ov, &BOB, true).unwrap();
    freeze(&mut ov, &ALICE, true).unwrap();
    builtin::set_paused(&mut ov, &asset(), &ISSUER, true).unwrap();
    assert_eq!(
        failed(builtin::renounce(&mut ov, &asset(), &ISSUER, 0x10)),
        "undefined capability bits"
    );
    // giving up the pause resumes the asset, and pausing is then refused for good
    builtin::renounce(&mut ov, &asset(), &ISSUER, CAP_PAUSE).unwrap();
    assert_eq!(
        (recorded(&ov).capabilities, recorded(&ov).paused),
        (CAP_MINT | CAP_FREEZE | CAP_ALLOWLIST, false)
    );
    assert_eq!(
        failed(builtin::set_paused(&mut ov, &asset(), &ISSUER, true)),
        "asset does not have the pause capability"
    );
    // ALICE's flag stays recorded but counts no more once freezing is given up; renouncing twice changes nothing
    assert_eq!(
        failed(send(&mut ov, &ALICE, &BOB, 1)),
        "rejected: sender is frozen for this asset"
    );
    builtin::renounce(&mut ov, &asset(), &ISSUER, CAP_FREEZE).unwrap();
    builtin::renounce(&mut ov, &asset(), &ISSUER, CAP_FREEZE).unwrap();
    assert_eq!(
        builtin::asset_status(&ov, &asset(), &ALICE).unwrap(),
        STATUS_FROZEN | STATUS_ALLOWED
    );
    send(&mut ov, &ALICE, &BOB, 1).unwrap();
    assert_eq!(
        failed(freeze(&mut ov, &ALICE, true)),
        "asset does not have the freeze capability"
    );
    // without the allowlist anyone receives; without minting the supply is fixed; nothing is left to give up
    let carol = [0xC0; 32];
    assert_eq!(
        failed(send(&mut ov, &BOB, &carol, 1)),
        "rejected: recipient is not on this asset's allowlist"
    );
    builtin::renounce(&mut ov, &asset(), &ISSUER, CAP_ALLOWLIST | CAP_MINT).unwrap();
    send(&mut ov, &BOB, &carol, 1).unwrap();
    assert_eq!(recorded(&ov).capabilities, 0);
    builtin::renounce(&mut ov, &asset(), &ISSUER, CAPABILITIES).unwrap();
    assert_eq!(recorded(&ov).capabilities, 0);
}

#[test]
fn a_status_key_is_deleted_when_no_flag_is_left() {
    let mut ov = setup(CAP_FREEZE | CAP_ALLOWLIST);
    let key = keys::asset_status(&asset(), &ALICE);
    freeze(&mut ov, &ALICE, true).unwrap();
    assert_eq!(
        ov.get(&key).unwrap(),
        Some(vec![STATUS_FROZEN | STATUS_ALLOWED])
    );
    freeze(&mut ov, &ALICE, false).unwrap();
    assert_eq!(ov.get(&key).unwrap(), Some(vec![STATUS_ALLOWED]));
    allow(&mut ov, &ALICE, false).unwrap();
    assert_eq!(ov.get(&key).unwrap(), None);
    // clearing a flag never set writes nothing either
    allow(&mut ov, &BOB, false).unwrap();
    assert_eq!(ov.get(&keys::asset_status(&asset(), &BOB)).unwrap(), None);
}

#[test]
fn a_bar_needs_the_capability_and_never_meets_the_issuer() {
    let meta = |capabilities: u8| AssetMeta {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
        total_supply: 0,
        issuer: ISSUER,
        capabilities,
        paused: false,
    };
    for capabilities in 0..=CAPABILITIES {
        for status in 0..=(STATUS_FROZEN | STATUS_ALLOWED) {
            let m = meta(capabilities);
            assert_eq!(builtin::bar(&m, &ISSUER, status), None);
            let expected = if capabilities & CAP_FREEZE != 0 && status & STATUS_FROZEN != 0 {
                Some(Bar::Frozen)
            } else if capabilities & CAP_ALLOWLIST != 0 && status & STATUS_ALLOWED == 0 {
                Some(Bar::NotAllowed)
            } else {
                None
            };
            assert_eq!(
                builtin::bar(&m, &ALICE, status),
                expected,
                "{capabilities} {status}"
            );
        }
    }
}
