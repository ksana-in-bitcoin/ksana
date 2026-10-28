//! The asset management host functions of D-121 (docs/ABI.md section 3): `asset_create`, `asset_freeze`, `asset_allow`,
//! `asset_pause` and `asset_renounce` name the current contract's own asset, check their arguments and charge their gas
//! before the host does anything; `asset_policy` and `asset_status` read what the host records.
mod common;

use common::{id, module, MockHost};
use ksana_primitives::asset::{
    CAPABILITIES, CAP_ALLOWLIST, CAP_FREEZE, CAP_MINT, CAP_PAUSE, STATUS_ALLOWED, STATUS_FROZEN,
};
use ksana_primitives::hash::sha256;
use ksana_primitives::DEVNET;
use ksana_vm::{
    gas, AssetMetaInput, AssetPolicy, CallOutcome, CallRequest, FailureReason, Vm, VmError,
};

const GAS: u64 = 10_000_000;

/// The `sub_id` 7…7 at 600, the metadata `{ name: "T", symbol: "T", decimals: 0 }` at 640 (11 bytes, a 12th byte after
/// it), the amount 1 000 at 660, the account 0xBB…BB at 700 and the account 0xCC…CC at 740.
const DATA: &str = r#"
  (data (i32.const 600) "\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07")
  (data (i32.const 640) "\01\00\00\00T\01\00\00\00T\00\ff")
  (data (i32.const 660) "\e8\03\00\00\00\00\00\00\00\00\00\00\00\00\00\00")
  (data (i32.const 700) "\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb")
  (data (i32.const 740) "\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc\cc")
"#;

/// A contract whose `ksana_call` runs `calls` and returns `Ok` of the `len` bytes at 900 (the arguments start at
/// `$a`).
fn contract(calls: &str, len: u32) -> Vec<u8> {
    module(&format!(
        r#"{DATA}
      (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
        (local $a i32)
        (local.set $a (call $args_ptr (local.get $p)))
        {calls}
        (call $ok (i32.const 900) (i32.const {len})))"#
    ))
}

fn create_call(capabilities: i32, meta_len: i32) -> String {
    format!(
        "(call $asset_create (i32.const 600) (i32.const 640) (i32.const {meta_len}) (i32.const {capabilities}) \
         (i32.const 700) (i32.const 660) (i32.const 900))"
    )
}

fn meta() -> AssetMetaInput {
    AssetMetaInput {
        name: "T".into(),
        symbol: "T".into(),
        decimals: 0,
    }
}

/// The asset with `sub_id` 7…7 of the contract `id(contract)`.
fn asset_of(contract: u8) -> [u8; 32] {
    let mut preimage = id(contract).to_vec();
    preimage.extend([7u8; 32]);
    sha256(&preimage)
}

/// A host with `calls` deployed as the contract `id(1)` and `id(2)`, and the asset of `id(1)` existing with
/// `capabilities`.
fn host(calls: &str, len: u32, capabilities: u8) -> MockHost {
    let mut host = MockHost::default();
    host.deploy(id(1), contract(calls, len));
    host.deploy(id(2), contract(calls, len));
    host.assets.insert(asset_of(1), (meta(), 0, id(1)));
    host.policies.insert(
        asset_of(1),
        AssetPolicy {
            capabilities,
            paused: false,
        },
    );
    host
}

fn call(
    vm: &Vm,
    host: MockHost,
    contract: u8,
    args: &[u8],
    gas: u64,
) -> (MockHost, Result<CallOutcome, VmError>) {
    vm.call(
        host,
        CallRequest {
            contract: id(contract),
            caller: id(0xAA),
            method: "x".into(),
            args: args.to_vec(),
            funds: Vec::new(),
            allow_init: false,
        },
        gas,
    )
}

fn success(r: Result<CallOutcome, VmError>) -> (Vec<u8>, u64) {
    match r {
        Ok(CallOutcome::Success { output, gas_used }) => (output, gas_used),
        other => panic!("should succeed, got {other:?}"),
    }
}

fn rejected(r: Result<CallOutcome, VmError>) -> String {
    match r {
        Ok(CallOutcome::Failure(FailureReason::Rejected(msg))) => msg,
        other => panic!("should be rejected, got {other:?}"),
    }
}

#[test]
fn asset_create_names_the_own_asset_and_records_its_capabilities() {
    let vm = Vm::new(&DEVNET).unwrap();
    let mut host = MockHost::default();
    host.deploy(
        id(1),
        contract(&create_call(i32::from(CAPABILITIES), 11), 32),
    );
    let (host, r) = call(&vm, host, 1, b"", GAS);
    let asset = asset_of(1);
    assert_eq!(
        success(r).0,
        asset.to_vec(),
        "asset ID = sha256(issuing contract ‖ sub_id)"
    );
    assert_eq!(host.assets[&asset], (meta(), 1_000, id(1)));
    assert_eq!(
        host.policies[&asset],
        AssetPolicy {
            capabilities: CAP_MINT | CAP_FREEZE | CAP_ALLOWLIST | CAP_PAUSE,
            paused: false
        }
    );
    assert_eq!(host.balances[&(asset, id(0xBB))], 1_000);
    // the host refuses an asset that exists
    let (host, r) = call(&vm, host, 1, b"", GAS);
    assert_eq!(rejected(r), "asset already exists");
    assert_eq!(host.admin, ["create 15"]);
}

#[test]
fn malformed_arguments_fail_before_the_host_is_asked() {
    let vm = Vm::new(&DEVNET).unwrap();
    for (calls, expected) in [
        (create_call(256, 11), "capabilities out of range"),
        (create_call(-1, 11), "capabilities out of range"),
        (
            create_call(0, 12),
            "meta of asset_create is not a valid encoding",
        ),
        (create_call(0, 1_025), "length exceeds the limit"),
        (
            "(call $asset_freeze (i32.const 600) (i32.const 700) (i32.const 2))".into(),
            "a switch argument must be 0 or 1",
        ),
        (
            "(call $asset_allow (i32.const 600) (i32.const 700) (i32.const -1))".into(),
            "a switch argument must be 0 or 1",
        ),
        (
            "(call $asset_pause (i32.const 600) (i32.const 2))".into(),
            "a switch argument must be 0 or 1",
        ),
        (
            "(call $asset_renounce (i32.const 600) (i32.const 256))".into(),
            "capabilities out of range",
        ),
    ] {
        let (host, r) = call(&vm, host(&calls, 0, CAPABILITIES), 1, b"", GAS);
        assert_eq!(rejected(r), expected, "{calls}");
        assert!(host.admin.is_empty(), "{calls}: {:?}", host.admin);
    }
}

#[test]
fn freeze_and_allow_set_the_flags_of_the_own_asset_only() {
    let vm = Vm::new(&DEVNET).unwrap();
    let calls = "(call $asset_freeze (i32.const 600) (i32.const 700) (i32.const 1))
                 (call $asset_allow (i32.const 600) (i32.const 740) (i32.const 1))
                 (call $asset_freeze (i32.const 600) (i32.const 700) (i32.const 0))";
    let (host, r) = call(&vm, host(calls, 0, CAP_FREEZE | CAP_ALLOWLIST), 1, b"", GAS);
    success(r);
    let asset = asset_of(1);
    assert_eq!(
        host.admin,
        [
            format!("status {STATUS_FROZEN} true"),
            format!("status {STATUS_ALLOWED} true"),
            format!("status {STATUS_FROZEN} false"),
        ]
    );
    assert_eq!(host.statuses[&(asset, id(0xBB))], 0);
    assert_eq!(host.statuses[&(asset, id(0xCC))], STATUS_ALLOWED);
    // the same code in another contract names that contract's asset, which does not exist: no contract reaches the
    // asset of another
    let (host, r) = call(&vm, host, 2, b"", GAS);
    assert_eq!(rejected(r), "asset does not exist");
    assert_eq!(host.admin.len(), 3);
}

#[test]
fn pause_and_renounce_reach_the_host() {
    let vm = Vm::new(&DEVNET).unwrap();
    let calls = "(call $asset_pause (i32.const 600) (i32.const 1))
                 (call $asset_renounce (i32.const 600) (i32.const 8))";
    let (host, r) = call(&vm, host(calls, 0, CAP_MINT | CAP_PAUSE), 1, b"", GAS);
    success(r);
    assert_eq!(host.admin, ["pause true", "renounce 8"]);
    assert_eq!(
        host.policies[&asset_of(1)],
        AssetPolicy {
            capabilities: CAP_MINT,
            paused: true
        }
    );
}

#[test]
fn asset_policy_returns_the_capabilities_and_the_pause() {
    let vm = Vm::new(&DEVNET).unwrap();
    let calls = "(i64.store (i32.const 900) (call $asset_policy (local.get $a)))";
    let policy = |host: MockHost, asset: [u8; 32]| {
        let (_, r) = call(&vm, host, 1, &asset, GAS);
        i64::from_le_bytes(success(r).0.try_into().unwrap())
    };
    let mut h = host(calls, 8, CAP_MINT | CAP_ALLOWLIST);
    assert_eq!(policy(h, asset_of(1)), 5);
    h = host(calls, 8, CAP_FREEZE | CAP_PAUSE);
    h.policies.get_mut(&asset_of(1)).unwrap().paused = true;
    assert_eq!(policy(h, asset_of(1)), 0x100 | 10);
    h = host(calls, 8, 0);
    assert_eq!(policy(h, asset_of(1)), 0);
    // -1 when the asset does not exist
    assert_eq!(policy(host(calls, 8, 0), asset_of(2)), -1);
}

#[test]
fn asset_status_returns_the_recorded_flags() {
    let vm = Vm::new(&DEVNET).unwrap();
    let calls = "(i32.store (i32.const 900)
                   (call $asset_status (local.get $a) (i32.add (local.get $a) (i32.const 32))))";
    let mut h = host(calls, 4, CAP_FREEZE | CAP_ALLOWLIST);
    h.statuses
        .insert((asset_of(1), id(0xBB)), STATUS_FROZEN | STATUS_ALLOWED);
    let args = |account: u8| [asset_of(1), id(account)].concat();
    let (h, r) = call(&vm, h, 1, &args(0xBB), GAS);
    assert_eq!(success(r).0, 3u32.to_le_bytes());
    let (_, r) = call(&vm, h, 1, &args(0xCC), GAS);
    assert_eq!(success(r).0, 0u32.to_le_bytes());
}

/// Each function with its charge for the calls of [`charged_calls`].
fn charged_calls() -> [(String, u64); 7] {
    [
        (
            create_call(0, 11),
            gas::ASSET_CREATE_BASE + 11 * gas::ASSET_CREATE_PER_BYTE,
        ),
        (
            "(call $asset_freeze (i32.const 600) (i32.const 700) (i32.const 1))".into(),
            gas::ASSET_SET_STATUS,
        ),
        (
            "(call $asset_allow (i32.const 600) (i32.const 700) (i32.const 1))".into(),
            gas::ASSET_SET_STATUS,
        ),
        (
            "(call $asset_pause (i32.const 600) (i32.const 0))".into(),
            gas::ASSET_PAUSE,
        ),
        (
            "(call $asset_renounce (i32.const 600) (i32.const 0))".into(),
            gas::ASSET_RENOUNCE,
        ),
        (
            "(drop (call $asset_policy (i32.const 600)))".into(),
            gas::ASSET_POLICY,
        ),
        (
            "(drop (call $asset_status (i32.const 600) (i32.const 700)))".into(),
            gas::ASSET_STATUS,
        ),
    ]
}

/// A host for `calls` in which they succeed: the asset of `id(1)` exists with every capability, unless `calls`
/// creates it.
fn ready(calls: &str) -> MockHost {
    let mut h = host(calls, 0, CAPABILITIES);
    if calls.contains("asset_create") {
        h.assets.remove(&asset_of(1));
        h.policies.remove(&asset_of(1));
    }
    h
}

#[test]
fn each_function_charges_its_gas() {
    let vm = Vm::new(&DEVNET).unwrap();
    let used = |calls: &str| success(call(&vm, ready(calls), 1, b"", GAS).1).1;
    for (one, charge) in charged_calls() {
        // one call more costs the charge and its few instructions; a second creation would fail, so a creation is
        // measured against no call at all
        let (before, after) = if one.contains("asset_create") {
            (used(""), used(&one))
        } else {
            (used(&one), used(&format!("{one} {one}")))
        };
        let extra = after - before;
        assert!(
            (charge..charge + 40).contains(&extra),
            "{one}: {extra} for a charge of {charge}"
        );
    }
}

#[test]
fn the_host_acts_only_once_the_charge_is_paid() {
    let vm = Vm::new(&DEVNET).unwrap();
    // the functions that change the asset: the host logs them
    for (one, charge) in charged_calls().into_iter().take(5) {
        let asked = |limit: u64| !call(&vm, ready(&one), 1, b"", limit).0.admin.is_empty();
        let (_, r) = call(&vm, ready(&one), 1, b"", GAS);
        let need = success(r).1;
        assert!(asked(need));
        // the least limit at which the host acts
        let (mut lo, mut hi) = (0, need);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if asked(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        // it acts only when the charge is covered: no more than a few instructions after the call are left unpaid
        assert!(
            need - lo < 100 && lo > charge,
            "{one}: the host acts at {lo} of {need}"
        );
    }
}
