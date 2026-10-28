//! Semantics of the in-memory mock host (consistent with docs/ABI.md).
#![cfg(not(target_arch = "wasm32"))]

use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{
    self as sdk, decode, encode, ok, revert, storage, AssetMetaInput, CallResult, Funds,
};

const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];

/// Test contract: exposes the host functions as methods.
fn probe(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let tag: Vec<u8> = decode(args)?;
            if tag == b"fail" {
                return revert("init failed");
            }
            storage::set(b"init", &tag);
            ok(&())
        }
        "set" => {
            let (key, value): (Vec<u8>, Vec<u8>) = decode(args)?;
            storage::set(&key, &value);
            ok(&())
        }
        "get" => {
            let key: Vec<u8> = decode(args)?;
            ok(&storage::get(&key))
        }
        "remove" => {
            let key: Vec<u8> = decode(args)?;
            storage::remove(&key);
            ok(&())
        }
        "whoami" => ok(&(sdk::caller(), sdk::self_id())),
        "heights" => ok(&(sdk::block_height(), sdk::btc_anchor_height())),
        "btc_header" => {
            let height: u32 = decode(args)?;
            ok(&sdk::btc_header(height).to_vec())
        }
        "call" => {
            let (target, method, inner): ([u8; 32], String, Vec<u8>) = decode(args)?;
            ok(&sdk::call(&target, &method, &inner))
        }
        "set_then_call" => {
            let (target, method, inner): ([u8; 32], String, Vec<u8>) = decode(args)?;
            storage::set(b"before_call", b"1");
            sdk::emit_event(b"before_call");
            ok(&sdk::call(&target, &method, &inner))
        }
        "depth" => {
            // Self-call n levels deep: depth(n) -> depth(n - 1) -> … -> depth(0).
            let n: u32 = decode(args)?;
            if n == 0 {
                return ok(&0u32);
            }
            let inner: u32 = decode(&sdk::call(&sdk::self_id(), "depth", &encode(&(n - 1))))?;
            ok(&(inner + 1))
        }
        "revert" => revert("nope"),
        "panic" => panic!("boom"),
        "big_output" => Ok(vec![0u8; 65_537]),
        "emit" => {
            let data: Vec<u8> = decode(args)?;
            sdk::emit_event(&data);
            ok(&())
        }
        "issue" => {
            let (sub_id, name, decimals, to, amount): ([u8; 32], String, u8, [u8; 32], u128) =
                decode(args)?;
            let symbol = name.chars().take(3).collect();
            ok(&sdk::asset_issue(
                &sub_id,
                &AssetMetaInput {
                    name,
                    symbol,
                    decimals,
                },
                &to,
                amount,
            ))
        }
        "issue_meta" => {
            let (sub_id, meta, to, amount): ([u8; 32], AssetMetaInput, [u8; 32], u128) =
                decode(args)?;
            ok(&sdk::asset_issue(&sub_id, &meta, &to, amount))
        }
        "create_asset" => {
            let (sub_id, meta, caps, to, amount): ([u8; 32], AssetMetaInput, u8, [u8; 32], u128) =
                decode(args)?;
            ok(&sdk::asset_create(&sub_id, &meta, caps, &to, amount))
        }
        "freeze" => {
            let (sub_id, account, on): ([u8; 32], [u8; 32], bool) = decode(args)?;
            sdk::asset_freeze(&sub_id, &account, on);
            ok(&())
        }
        "allow" => {
            let (sub_id, account, on): ([u8; 32], [u8; 32], bool) = decode(args)?;
            sdk::asset_allow(&sub_id, &account, on);
            ok(&())
        }
        "pause" => {
            let (sub_id, on): ([u8; 32], bool) = decode(args)?;
            sdk::asset_pause(&sub_id, on);
            ok(&())
        }
        "renounce" => {
            let (sub_id, caps): ([u8; 32], u8) = decode(args)?;
            sdk::asset_renounce(&sub_id, caps);
            ok(&())
        }
        "policy" => {
            let asset: [u8; 32] = decode(args)?;
            ok(&sdk::asset_policy(&asset).map(|p| (p.capabilities, p.paused)))
        }
        "status" => {
            let (asset, account): ([u8; 32], [u8; 32]) = decode(args)?;
            ok(&sdk::asset_status(&asset, &account))
        }
        "transfer_asset" => {
            let (asset, to, amount): ([u8; 32], [u8; 32], u128) = decode(args)?;
            sdk::asset_transfer(&asset, &to, amount);
            ok(&())
        }
        "balance" => {
            let (asset, account): ([u8; 32], [u8; 32]) = decode(args)?;
            ok(&sdk::asset_balance(&asset, &account))
        }
        "funds" => ok(&(sdk::call_funds(), sdk::received(&sdk::KSA_ASSET_ID))),
        "try_call" => {
            let (target, method, inner, funds): ([u8; 32], String, Vec<u8>, Vec<Funds>) =
                decode(args)?;
            let result = sdk::try_call_with(&target, &method, &inner, &funds, 0);
            // The caller continues after the callee fails; its own writes are kept
            storage::set(b"after_try", b"1");
            ok(&match result {
                Ok(data) => (0u8, data),
                Err(sdk::CallError::Revert(reason)) => (1, reason),
                Err(sdk::CallError::Failed) => (2, Vec::new()),
            })
        }
        "delegate" => {
            let (target, method, inner): ([u8; 32], String, Vec<u8>) = decode(args)?;
            ok(&sdk::delegate_call(&target, &method, &inner))
        }
        "env" => ok(&(sdk::block_timestamp(), sdk::chain_id(), sdk::gas_left())),
        "burn" => {
            let (asset, amount): ([u8; 32], u128) = decode(args)?;
            sdk::asset_burn(&asset, amount);
            ok(&())
        }
        "create" => {
            let (code_hash, salt, init_args, funds): ([u8; 32], [u8; 32], Vec<u8>, Vec<Funds>) =
                decode(args)?;
            ok(&sdk::create_contract(&code_hash, &salt, &init_args, &funds))
        }
        "call_with_funds" => {
            let (target, method, inner, funds): ([u8; 32], String, Vec<u8>, Vec<Funds>) =
                decode(args)?;
            ok(&sdk::call_with_funds(&target, &method, &inner, &funds))
        }
        _ => revert("unknown method"),
    }
}

fn deploy_probe(nonce: u64) -> [u8; 32] {
    mock::deploy(ALICE, nonce, b"probe", probe, &encode(&b"ok".to_vec())).unwrap()
}

fn call(contract: [u8; 32], method: &str, args: &[u8]) -> Result<Vec<u8>, Failure> {
    mock::call(ALICE, contract, method, args)
}

fn trap_reason(result: Result<Vec<u8>, Failure>) -> String {
    match result {
        Err(Failure::Trap(reason)) => reason,
        other => panic!("expected a trap, got {other:?}"),
    }
}

#[test]
fn deploy_computes_contract_id_and_runs_init() {
    mock::reset();
    let id = deploy_probe(7);
    let mut preimage = ALICE.to_vec();
    preimage.extend_from_slice(&7u64.to_le_bytes());
    preimage.extend_from_slice(&sdk::sha256(b"probe"));
    assert_eq!(id, sdk::sha256(&preimage));
    assert_eq!(id, sdk::contract_id(&ALICE, 7, &sdk::sha256(b"probe")));
    assert_eq!(mock::storage_value(&id, b"init"), Some(b"ok".to_vec()));
    // The same (deployer, nonce, code) cannot be deployed again.
    assert!(mock::deploy(ALICE, 7, b"probe", probe, &encode(&b"ok".to_vec())).is_err());
}

#[test]
fn failed_init_undoes_deployment() {
    mock::reset();
    let result = mock::deploy(ALICE, 0, b"probe", probe, &encode(&b"fail".to_vec()));
    let id = sdk::contract_id(&ALICE, 0, &sdk::sha256(b"probe"));
    assert_eq!(
        result,
        Err(Failure::Revert {
            contract: id,
            reason: b"init failed".to_vec()
        })
    );
    assert!(!mock::contract_exists(&id));
}

#[test]
fn init_is_only_callable_at_deployment() {
    mock::reset();
    let id = deploy_probe(0);
    let reason = trap_reason(call(id, "init", &encode(&b"again".to_vec())));
    assert!(reason.contains("init"), "{reason}");
    // An inter-contract call of init fails as well.
    let other = deploy_probe(1);
    let args = encode(&(id, "init".to_string(), encode(&b"x".to_vec())));
    assert!(trap_reason(call(other, "call", &args)).contains("init"));
}

#[test]
fn storage_is_per_contract_and_bounded() {
    mock::reset();
    let a = deploy_probe(0);
    let b = deploy_probe(1);
    call(a, "set", &encode(&(b"k".to_vec(), b"va".to_vec()))).unwrap();
    call(b, "set", &encode(&(b"k".to_vec(), b"vb".to_vec()))).unwrap();
    let get = |c, k: &[u8]| {
        decode::<Option<Vec<u8>>>(&call(c, "get", &encode(&k.to_vec())).unwrap()).unwrap()
    };
    assert_eq!(get(a, b"k"), Some(b"va".to_vec()));
    assert_eq!(get(b, b"k"), Some(b"vb".to_vec()));
    assert_eq!(get(a, b"missing"), None);
    // An empty value counts as "present".
    call(a, "set", &encode(&(b"empty".to_vec(), Vec::<u8>::new()))).unwrap();
    assert_eq!(get(a, b"empty"), Some(vec![]));
    call(a, "remove", &encode(&b"k".to_vec())).unwrap();
    assert_eq!(get(a, b"k"), None);

    // Limits: key 256 bytes, value 65,536 bytes.
    call(a, "set", &encode(&(vec![1u8; 256], vec![2u8; 65_536]))).unwrap();
    assert!(
        trap_reason(call(a, "set", &encode(&(vec![1u8; 257], Vec::<u8>::new())))).contains("key")
    );
    assert!(
        trap_reason(call(a, "set", &encode(&(vec![1u8; 1], vec![0u8; 65_537])))).contains("value")
    );
    assert!(trap_reason(call(a, "get", &encode(&vec![1u8; 257]))).contains("key"));
}

#[test]
fn caller_and_self_id_follow_the_call_chain() {
    mock::reset();
    let a = deploy_probe(0);
    let b = deploy_probe(1);
    let top: ([u8; 32], [u8; 32]) = decode(&call(a, "whoami", &[]).unwrap()).unwrap();
    assert_eq!(top, (ALICE, a));
    let nested = call(
        a,
        "call",
        &encode(&(b, "whoami".to_string(), Vec::<u8>::new())),
    )
    .unwrap();
    let inner: ([u8; 32], [u8; 32]) = decode(&decode::<Vec<u8>>(&nested).unwrap()).unwrap();
    assert_eq!(
        inner,
        (a, b),
        "the caller of a nested call is the calling contract"
    );
}

#[test]
fn heights() {
    mock::reset();
    let a = deploy_probe(0);
    mock::set_block_height(42);
    mock::set_btc_anchor_height(900);
    assert_eq!(
        decode::<(u64, u32)>(&call(a, "heights", &[]).unwrap()).unwrap(),
        (42, 900)
    );
}

#[test]
fn btc_header_requires_depth() {
    mock::reset();
    let a = deploy_probe(0);
    for h in 90..=100 {
        mock::set_btc_header(h, [u8::try_from(h).unwrap(); 80]);
    }
    mock::set_btc_anchor_height(100);
    // anchor - BTC_QUERY_DEPTH = 94 is the most recent readable height.
    let header = decode::<Vec<u8>>(&call(a, "btc_header", &encode(&94u32)).unwrap()).unwrap();
    assert_eq!(header, vec![94u8; 80]);
    assert!(trap_reason(call(a, "btc_header", &encode(&95u32))).contains("deep"));
    assert!(trap_reason(call(a, "btc_header", &encode(&100u32))).contains("deep"));
    assert!(trap_reason(call(a, "btc_header", &encode(&50u32))).contains("unknown"));
    // When anchor is less than the depth, nothing is readable.
    mock::set_btc_anchor_height(5);
    assert!(trap_reason(call(a, "btc_header", &encode(&0u32))).contains("deep"));
    mock::set_btc_anchor_height(6);
    mock::set_btc_header(0, [7; 80]);
    assert!(call(a, "btc_header", &encode(&0u32)).is_ok());
}

#[test]
fn asset_issue_rules() {
    mock::reset();
    let a = deploy_probe(0);
    let sub_id = [5u8; 32];
    let issue = |name: &str, decimals: u8, to: [u8; 32], amount: u128| {
        call(
            a,
            "issue",
            &encode(&(sub_id, name.to_string(), decimals, to, amount)),
        )
    };
    let asset: [u8; 32] = decode(&issue("Gold", 8, BOB, 1_000).unwrap()).unwrap();
    assert_eq!(asset, sdk::asset_id(&a, &sub_id));
    assert_eq!(
        asset,
        sdk::sha256(&[a.as_slice(), sub_id.as_slice()].concat())
    );
    let info = mock::asset_info(&asset).unwrap();
    assert_eq!(
        (
            info.name.as_str(),
            info.decimals,
            info.total_supply,
            info.issuer
        ),
        ("Gold", 8, 1_000, a)
    );
    assert_eq!(mock::balance(&asset, &BOB), 1_000);
    issue("Gold", 8, ALICE, 5).unwrap();
    assert_eq!(mock::asset_info(&asset).unwrap().total_supply, 1_005);
    assert_eq!(mock::asset_info(&asset).unwrap().symbol, "Gol");
    assert_eq!(
        mock::asset_info(&asset).unwrap().capabilities,
        sdk::CAP_MINT
    );
    // The metadata must match the first issuance.
    assert!(trap_reason(issue("Silver", 8, BOB, 1)).contains("meta"));
    assert!(trap_reason(issue("Gold", 6, BOB, 1)).contains("meta"));
    let other_symbol = AssetMetaInput {
        name: "Gold".into(),
        symbol: "GLD".into(),
        decimals: 8,
    };
    assert!(trap_reason(call(
        a,
        "issue_meta",
        &encode(&(sub_id, other_symbol, BOB, 1u128))
    ))
    .contains("meta"));
    // Total supply overflow.
    assert!(trap_reason(issue("Gold", 8, BOB, u128::MAX)).contains("overflow"));
    assert_eq!(mock::asset_info(&asset).unwrap().total_supply, 1_005);
    // Different contracts with the same sub_id are different assets.
    let b = deploy_probe(1);
    let other: [u8; 32] = decode(
        &call(
            b,
            "issue",
            &encode(&(sub_id, "Gold".to_string(), 8u8, BOB, 1u128)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(other, asset);
}

#[test]
fn asset_meta_is_bounded() {
    mock::reset();
    let a = deploy_probe(0);
    let meta = |name: &str, symbol: &str, decimals: u8| AssetMetaInput {
        name: name.into(),
        symbol: symbol.into(),
        decimals,
    };
    let issue = |sub: u8, meta: AssetMetaInput| {
        call(a, "issue_meta", &encode(&([sub; 32], meta, BOB, 1u128)))
    };
    // The limits of D-121: a name of 1 to 64 bytes, a symbol of 1 to 16 bytes, at most 18 decimals.
    issue(1, meta(&"n".repeat(64), &"s".repeat(16), 18)).unwrap();
    issue(2, meta("ñ", "₿", 0)).unwrap();
    assert!(trap_reason(issue(3, meta(&"n".repeat(65), "S", 0))).contains("name"));
    assert!(trap_reason(issue(3, meta("", "S", 0))).contains("name"));
    assert!(trap_reason(issue(3, meta("N", &"s".repeat(17), 0))).contains("symbol"));
    assert!(trap_reason(issue(3, meta("N", "", 0))).contains("symbol"));
    assert!(trap_reason(issue(3, meta("N", "S", 19))).contains("decimals"));
    // An encoding over 1,024 bytes traps before it is decoded.
    assert!(trap_reason(issue(3, meta(&"n".repeat(1_020), "S", 0))).contains("too long"));
    assert!(mock::asset_info(&sdk::asset_id(&a, &[3; 32])).is_none());
}

/// `create` arguments: this test's asset `sub_id` 9 named "Dollar" / "USDX" with 6 decimals.
fn create_args(caps: u8, to: [u8; 32], amount: u128) -> Vec<u8> {
    let meta = AssetMetaInput {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
    };
    encode(&([9u8; 32], meta, caps, to, amount))
}

#[test]
fn asset_create_declares_capabilities_for_good() {
    mock::reset();
    let a = deploy_probe(0);
    let asset: [u8; 32] =
        decode(&call(a, "create_asset", &create_args(0, BOB, 1_000)).unwrap()).unwrap();
    assert_eq!(asset, sdk::asset_id(&a, &[9; 32]));
    let info = mock::asset_info(&asset).unwrap();
    assert_eq!(
        (
            info.symbol.as_str(),
            info.capabilities,
            info.paused,
            info.total_supply
        ),
        ("USDX", 0, false, 1_000)
    );
    assert_eq!(mock::balance(&asset, &BOB), 1_000);
    // Without CAP_MINT the supply is fixed, and no asset is created twice.
    let meta = AssetMetaInput {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
    };
    assert_eq!(
        trap_reason(call(
            a,
            "issue_meta",
            &encode(&([9u8; 32], meta, BOB, 1u128))
        )),
        "rejected: asset supply is fixed"
    );
    assert!(trap_reason(call(a, "create_asset", &create_args(1, BOB, 1))).contains("exists"));
    // Undefined bits, and capabilities that were never declared.
    assert!(trap_reason(call(
        a,
        "create_asset",
        &encode(&(
            [8u8; 32],
            AssetMetaInput {
                name: "X".into(),
                symbol: "X".into(),
                decimals: 0,
            },
            16u8,
            BOB,
            0u128
        ))
    ))
    .contains("undefined"));
    assert!(
        trap_reason(call(a, "freeze", &encode(&([9u8; 32], BOB, true)))).contains("capability")
    );
    assert!(trap_reason(call(a, "allow", &encode(&([9u8; 32], BOB, true)))).contains("capability"));
    assert!(trap_reason(call(a, "pause", &encode(&([9u8; 32], true)))).contains("capability"));
    // Clearing a flag or resuming never needs a capability; the issuing contract has no status.
    call(a, "freeze", &encode(&([9u8; 32], BOB, false))).unwrap();
    call(a, "pause", &encode(&([9u8; 32], false))).unwrap();
    assert!(trap_reason(call(a, "freeze", &encode(&([9u8; 32], a, false)))).contains("issuing"));
    // Only the issuing contract names the asset: another contract's sub_id 9 is another (missing) asset.
    let b = deploy_probe(1);
    assert!(
        trap_reason(call(b, "freeze", &encode(&([9u8; 32], BOB, false))))
            .contains("does not exist")
    );
    // Anyone reads the policy and the status.
    assert_eq!(
        decode::<Option<(u8, bool)>>(&call(b, "policy", &encode(&asset)).unwrap()).unwrap(),
        Some((0, false))
    );
    assert_eq!(
        decode::<Option<(u8, bool)>>(&call(b, "policy", &encode(&[7u8; 32])).unwrap()).unwrap(),
        None
    );
}

#[test]
fn frozen_paused_and_allowlisted_assets_refuse_movements() {
    mock::reset();
    let a = deploy_probe(0);
    let caps = sdk::CAP_MINT | sdk::CAP_FREEZE | sdk::CAP_ALLOWLIST | sdk::CAP_PAUSE;
    // With CAP_ALLOWLIST only the issuing contract receives before it allows anyone.
    assert_eq!(
        trap_reason(call(a, "create_asset", &create_args(caps, BOB, 1))),
        "rejected: recipient is not on this asset's allowlist"
    );
    let asset: [u8; 32] =
        decode(&call(a, "create_asset", &create_args(caps, a, 1_000)).unwrap()).unwrap();
    let sub = [9u8; 32];
    let b = deploy_probe(1);
    let send = |from: [u8; 32], to: [u8; 32], amount: u128| {
        call(from, "transfer_asset", &encode(&(asset, to, amount)))
    };
    assert_eq!(
        trap_reason(send(a, b, 10)),
        "rejected: recipient is not on this asset's allowlist"
    );
    call(a, "allow", &encode(&(sub, b, true))).unwrap();
    send(a, b, 10).unwrap();
    assert_eq!(mock::account_status(&asset, &b), sdk::STATUS_ALLOWED);
    assert_eq!(
        decode::<u8>(&call(b, "status", &encode(&(asset, b))).unwrap()).unwrap(),
        sdk::STATUS_ALLOWED
    );
    // b sends back to the issuing contract, which is always allowed; not to an account that is not allowed.
    send(b, a, 1).unwrap();
    assert_eq!(
        trap_reason(send(b, BOB, 1)),
        "rejected: recipient is not on this asset's allowlist"
    );
    // A frozen account neither sends, receives nor burns; the flags combine.
    call(a, "freeze", &encode(&(sub, b, true))).unwrap();
    assert_eq!(
        mock::account_status(&asset, &b),
        sdk::STATUS_ALLOWED | sdk::STATUS_FROZEN
    );
    assert_eq!(
        trap_reason(send(b, a, 1)),
        "rejected: sender is frozen for this asset"
    );
    assert_eq!(
        trap_reason(send(a, b, 1)),
        "rejected: recipient is frozen for this asset"
    );
    assert_eq!(
        trap_reason(call(b, "burn", &encode(&(asset, 1u128)))),
        "rejected: sender is frozen for this asset"
    );
    // Issuing to a frozen account is refused too.
    let meta = AssetMetaInput {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
    };
    assert_eq!(
        trap_reason(call(
            a,
            "issue_meta",
            &encode(&(sub, meta.clone(), b, 1u128))
        )),
        "rejected: recipient is frozen for this asset"
    );
    call(a, "freeze", &encode(&(sub, b, false))).unwrap();
    send(b, a, 1).unwrap();
    // A pause stops every movement out of any account but the issuing contract's.
    call(a, "pause", &encode(&(sub, true))).unwrap();
    assert_eq!(
        decode::<Option<(u8, bool)>>(&call(b, "policy", &encode(&asset)).unwrap()).unwrap(),
        Some((caps, true))
    );
    assert_eq!(trap_reason(send(b, a, 1)), "rejected: asset is paused");
    send(a, b, 1).unwrap();
    call(a, "issue_meta", &encode(&(sub, meta, a, 5u128))).unwrap();
    // Attached funds follow the same rules.
    let pay = ksana_contract_sdk::Funds { asset, amount: 1 };
    assert_eq!(
        trap_reason(mock::call_with_funds(b, a, "funds", &[], &[pay])),
        "rejected: asset is paused"
    );
    // Renouncing PAUSE resumes the asset; renouncing FREEZE and ALLOWLIST lifts their flags for good.
    call(
        a,
        "renounce",
        &encode(&(sub, sdk::CAP_PAUSE | sdk::CAP_ALLOWLIST)),
    )
    .unwrap();
    assert!(!mock::asset_info(&asset).unwrap().paused);
    send(b, BOB, 1).unwrap();
    call(a, "freeze", &encode(&(sub, BOB, true))).unwrap();
    assert_eq!(
        trap_reason(send(b, BOB, 1)),
        "rejected: recipient is frozen for this asset"
    );
    call(a, "renounce", &encode(&(sub, sdk::CAP_FREEZE))).unwrap();
    send(b, BOB, 1).unwrap();
    assert!(trap_reason(call(a, "freeze", &encode(&(sub, BOB, true)))).contains("capability"));
    assert!(trap_reason(call(a, "pause", &encode(&(sub, true)))).contains("capability"));
    assert!(trap_reason(call(a, "renounce", &encode(&(sub, 0x80u8)))).contains("undefined"));
    // Renouncing a capability twice is harmless.
    call(a, "renounce", &encode(&(sub, sdk::CAP_FREEZE))).unwrap();
    assert_eq!(
        mock::asset_info(&asset).unwrap().capabilities,
        sdk::CAP_MINT
    );
}

#[test]
fn a_registered_asset_takes_a_policy_for_tests() {
    mock::reset();
    let a = deploy_probe(0);
    mock::register_asset([7; 32], "X", 0, [0x99; 32]);
    mock::mint([7; 32], a, 10).unwrap();
    mock::set_asset_policy([7; 32], sdk::CAP_FREEZE, false);
    mock::set_account_status([7; 32], BOB, sdk::STATUS_FROZEN);
    assert_eq!(
        trap_reason(call(a, "transfer_asset", &encode(&([7u8; 32], BOB, 1u128)))),
        "rejected: recipient is frozen for this asset"
    );
    mock::set_account_status([7; 32], BOB, 0);
    call(a, "transfer_asset", &encode(&([7u8; 32], BOB, 1u128))).unwrap();
    assert_eq!(mock::account_status(&[7; 32], &BOB), 0);
}

#[test]
fn asset_transfer_moves_only_the_contracts_own_funds() {
    mock::reset();
    let a = deploy_probe(0);
    mock::register_asset(sdk::KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(sdk::KSA_ASSET_ID, a, 100).unwrap();
    mock::mint(sdk::KSA_ASSET_ID, ALICE, 1_000).unwrap();
    call(
        a,
        "transfer_asset",
        &encode(&(sdk::KSA_ASSET_ID, BOB, 60u128)),
    )
    .unwrap();
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 40);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &BOB), 60);
    // The caller ALICE's balance cannot be used by the contract: insufficient balance of the contract itself traps.
    assert!(trap_reason(call(
        a,
        "transfer_asset",
        &encode(&(sdk::KSA_ASSET_ID, BOB, 41u128))
    ))
    .contains("insufficient"));
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &ALICE), 1_000);
    // A nonexistent asset.
    assert!(
        trap_reason(call(a, "transfer_asset", &encode(&([9u8; 32], BOB, 0u128))))
            .contains("asset does not exist")
    );
    // Balance query.
    let balance: u128 =
        decode(&call(a, "balance", &encode(&(sdk::KSA_ASSET_ID, BOB))).unwrap()).unwrap();
    assert_eq!(balance, 60);
    // Transfer to itself: the balance is unchanged.
    call(
        a,
        "transfer_asset",
        &encode(&(sdk::KSA_ASSET_ID, a, 40u128)),
    )
    .unwrap();
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 40);
}

#[test]
fn nested_revert_fails_the_whole_operation_and_rolls_back() {
    mock::reset();
    let a = deploy_probe(0);
    let b = deploy_probe(1);
    mock::clear_events();
    let result = call(
        a,
        "set_then_call",
        &encode(&(b, "revert".to_string(), Vec::<u8>::new())),
    );
    assert_eq!(
        result,
        Err(Failure::Revert {
            contract: b,
            reason: b"nope".to_vec()
        })
    );
    assert_eq!(mock::storage_value(&a, b"before_call"), None);
    assert!(mock::events().is_empty());

    // On success both the changes and the events are kept.
    call(
        a,
        "set_then_call",
        &encode(&(b, "whoami".to_string(), Vec::<u8>::new())),
    )
    .unwrap();
    assert_eq!(mock::storage_value(&a, b"before_call"), Some(b"1".to_vec()));
    assert_eq!(mock::events().len(), 1);
    assert_eq!(mock::events()[0].contract, a);
    assert_eq!(mock::events()[0].data, b"before_call".to_vec());
}

#[test]
fn panics_and_invalid_outputs_are_traps() {
    mock::reset();
    let a = deploy_probe(0);
    let b = deploy_probe(1);
    let result = call(
        a,
        "set_then_call",
        &encode(&(b, "panic".to_string(), Vec::<u8>::new())),
    );
    assert!(trap_reason(result).contains("boom"));
    assert_eq!(mock::storage_value(&a, b"before_call"), None);
    assert!(trap_reason(call(a, "big_output", &[])).contains("output"));
    assert!(trap_reason(call([0xee; 32], "whoami", &[])).contains("no contract"));
}

#[test]
fn call_depth_limit() {
    mock::reset();
    let a = deploy_probe(0);
    // The top level is level 1: depth(7) is 8 levels in total.
    assert_eq!(
        decode::<u32>(&call(a, "depth", &encode(&7u32)).unwrap()).unwrap(),
        7
    );
    assert!(trap_reason(call(a, "depth", &encode(&8u32))).contains("depth"));
    mock::set_max_call_depth(3);
    assert!(call(a, "depth", &encode(&2u32)).is_ok());
    assert!(call(a, "depth", &encode(&3u32)).is_err());
}

#[test]
fn reentrancy_sees_storage_changes_immediately() {
    mock::reset();
    let a = deploy_probe(0);
    let b = deploy_probe(1);
    // a.set_then_call(b.call(a.get("before_call"))): when b calls back into a it can read the value a just wrote.
    let read_back = encode(&(a, "get".to_string(), encode(&b"before_call".to_vec())));
    let out = call(
        a,
        "set_then_call",
        &encode(&(b, "call".to_string(), read_back)),
    )
    .unwrap();
    let via_b: Vec<u8> = decode(&out).unwrap();
    let from_a: Vec<u8> = decode(&via_b).unwrap();
    assert_eq!(
        decode::<Option<Vec<u8>>>(&from_a).unwrap(),
        Some(b"1".to_vec())
    );
}

#[test]
fn call_input_and_event_limits() {
    mock::reset();
    let a = deploy_probe(0);
    let b = deploy_probe(1);
    call(a, "emit", &encode(&vec![0u8; 16_384])).unwrap();
    assert!(trap_reason(call(a, "emit", &encode(&vec![0u8; 16_385]))).contains("event"));
    // The input of an inter-contract call exceeds MAX_OP_BYTES.
    let huge = encode(&(b, "whoami".to_string(), vec![0u8; 65_536]));
    assert!(trap_reason(call(a, "call", &huge)).contains("input"));
}

#[test]
fn direct_context_for_unit_tests() {
    mock::reset();
    let value = mock::with_context(ALICE, BOB, || {
        storage::set(b"k", b"v");
        (sdk::caller(), sdk::self_id(), storage::get(b"k"))
    });
    assert_eq!(value, (ALICE, BOB, Some(b"v".to_vec())));
    assert_eq!(mock::storage_value(&BOB, b"k"), Some(b"v".to_vec()));
}

#[test]
fn crypto_primitives() {
    assert_eq!(
        hex::encode(sdk::sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex::encode(sdk::sha256d(b"abc")),
        "4f8b42c22dd3729b519ba6f68d2da7cc5b2d606d05daed5ad5128cc03e6c6358"
    );
    // Rows 1, 5, 6 of the official BIP340 test vectors.
    let h = |s: &str| hex::decode(s).unwrap();
    let pubkey: [u8; 32] = h("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659")
        .try_into()
        .unwrap();
    let msg: [u8; 32] = h("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89")
        .try_into()
        .unwrap();
    let good: [u8; 64] = h("6896BD60EEAE296DB48A229FF71DFE071BDE413E6D43F917DC8DCF8C78DE33418906D11AC976ABCCB20B091292BFF4EA897EFCB639EA871CFA95F6DE339E4B0A").try_into().unwrap();
    let bad: [u8; 64] = h("FFF97BD5755EEEA420453A14355235D382F6472F8568A18B2F057A14602975563CC27944640AC607CD107AE10923D9EF7A73C643E166BE5EBEAFA34B1AC553E2").try_into().unwrap();
    let off_curve: [u8; 32] = h("EEFDEA4CDB677750A420FEE807EACF21EB9898AE79B9768766E4FAA04A2D4A34")
        .try_into()
        .unwrap();
    let sig5: [u8; 64] = h("6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E17776969E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B").try_into().unwrap();
    assert!(sdk::verify_schnorr(&pubkey, &msg, &good));
    assert!(!sdk::verify_schnorr(&pubkey, &msg, &bad));
    assert!(!sdk::verify_schnorr(&off_curve, &msg, &sig5));
}

fn ksa(amount: u128) -> Funds {
    Funds {
        asset: sdk::KSA_ASSET_ID,
        amount,
    }
}

/// Return value of the `funds` method: (attached funds, the KSA amount among them).
fn received(output: Vec<u8>) -> (Vec<Funds>, u128) {
    decode(&output).unwrap()
}

#[test]
fn call_funds_move_assets_into_the_callee_and_roll_back_on_failure() {
    mock::reset();
    mock::register_asset(sdk::KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(sdk::KSA_ASSET_ID, ALICE, 1_000).unwrap();
    let a = deploy_probe(0);
    let b = deploy_probe(1);

    // Top-level attachment: first transferred from the signing account to the contract, which reads the attached funds
    let out = mock::call_with_funds(ALICE, a, "funds", &[], &[ksa(100)]).unwrap();
    assert_eq!(received(out), (vec![ksa(100)], 100));
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &ALICE), 900);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 100);
    // Empty when nothing is attached
    assert_eq!(received(call(a, "funds", &[]).unwrap()), (vec![], 0));

    // Attachment on an inter-contract call: transferred from the current contract to the callee
    let args = encode(&(b, "funds".to_string(), Vec::<u8>::new(), vec![ksa(30)]));
    let out: Vec<u8> =
        decode(&mock::call_with_funds(ALICE, a, "call_with_funds", &args, &[ksa(50)]).unwrap())
            .unwrap();
    assert_eq!(received(out), (vec![ksa(30)], 30));
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &ALICE), 850);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 120);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &b), 30);

    // The callee fails: the attached transfers of both levels are rolled back together
    let args = encode(&(b, "revert".to_string(), Vec::<u8>::new(), vec![ksa(5)]));
    assert!(matches!(
        mock::call_with_funds(ALICE, a, "call_with_funds", &args, &[ksa(10)]),
        Err(Failure::Revert { .. })
    ));
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &ALICE), 850);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 120);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &b), 30);

    // Insufficient balance: of the signing account or of the transferring contract
    assert!(
        trap_reason(mock::call_with_funds(ALICE, a, "funds", &[], &[ksa(851)]))
            .contains("insufficient")
    );
    let args = encode(&(b, "funds".to_string(), Vec::<u8>::new(), vec![ksa(121)]));
    assert!(trap_reason(call(a, "call_with_funds", &args)).contains("insufficient"));
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 120);
}

#[test]
fn call_funds_rules() {
    mock::reset();
    mock::register_asset(sdk::KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::register_asset([1; 32], "X", 0, [0; 32]);
    mock::mint(sdk::KSA_ASSET_ID, ALICE, 1_000).unwrap();
    mock::mint([1; 32], ALICE, 1_000).unwrap();
    let a = deploy_probe(0);
    let x = |amount| Funds {
        asset: [1; 32],
        amount,
    };
    // Two assets in ascending order
    let out = mock::call_with_funds(ALICE, a, "funds", &[], &[ksa(1), x(2)]).unwrap();
    assert_eq!(received(out), (vec![ksa(1), x(2)], 1));
    // Out of order, duplicate, amount 0, more items than the limit
    for funds in [
        vec![x(1), ksa(1)],
        vec![ksa(1), ksa(2)],
        vec![ksa(0)],
        (0..=sdk::MAX_CALL_FUNDS)
            .map(|i| Funds {
                asset: [u8::try_from(i).unwrap(); 32],
                amount: 1,
            })
            .collect(),
    ] {
        assert!(mock::call_with_funds(ALICE, a, "funds", &[], &funds).is_err());
    }
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &ALICE), 999);
}

#[test]
fn deploy_with_funds_pays_the_new_contract_before_init() {
    mock::reset();
    mock::register_asset(sdk::KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(sdk::KSA_ASSET_ID, ALICE, 100).unwrap();
    let id = mock::deploy_with_funds(
        ALICE,
        0,
        b"probe",
        probe,
        &encode(&b"ok".to_vec()),
        &[ksa(40)],
    )
    .unwrap();
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &id), 40);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &ALICE), 60);
    // init fails: both the deployment and the attached transfer are undone
    let failed = mock::deploy_with_funds(
        ALICE,
        1,
        b"probe",
        probe,
        &encode(&b"fail".to_vec()),
        &[ksa(10)],
    );
    assert!(failed.is_err());
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &ALICE), 60);
}

/// Arguments and return value of the `try_call` method.
fn try_args(target: [u8; 32], method: &str, inner: Vec<u8>, funds: Vec<Funds>) -> Vec<u8> {
    encode(&(target, method.to_string(), inner, funds))
}

fn try_result(output: Vec<u8>) -> (u8, Vec<u8>) {
    decode(&output).unwrap()
}

#[test]
fn try_call_rolls_back_only_the_failed_layer() {
    mock::reset();
    mock::register_asset(sdk::KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(sdk::KSA_ASSET_ID, ALICE, 1_000).unwrap();
    let a = deploy_probe(0);
    let b = deploy_probe(1);
    let c = deploy_probe(2);

    // b writes storage and an event, then calls c, and c reverts: b's level fails as a whole, a catches it and
    // continues
    mock::clear_events();
    let inner = encode(&(c, "revert".to_string(), Vec::<u8>::new()));
    let out = mock::call_with_funds(
        ALICE,
        a,
        "try_call",
        &try_args(b, "set_then_call", inner, vec![ksa(40)]),
        &[ksa(100)],
    )
    .unwrap();
    assert_eq!(try_result(out), (1, b"nope".to_vec()));
    assert_eq!(mock::storage_value(&b, b"before_call"), None);
    assert!(
        mock::events().is_empty(),
        "the callee's events are rolled back"
    );
    assert_eq!(mock::storage_value(&a, b"after_try"), Some(b"1".to_vec()));
    // The funds attached to b return to a; the 100 a received is kept
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 100);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &b), 0);

    // Success: same as call
    let out = call(a, "try_call", &try_args(b, "whoami", vec![], vec![])).unwrap();
    let (status, data) = try_result(out);
    assert_eq!(status, 0);
    assert_eq!(decode::<([u8; 32], [u8; 32])>(&data).unwrap(), (a, b));

    // Trap, nonexistent contract, insufficient balance for the attached funds: failure with empty data
    for (target, method, funds) in [
        (b, "panic", vec![]),
        ([0x77; 32], "whoami", vec![]),
        (b, "whoami", vec![ksa(101)]),
    ] {
        let out = call(a, "try_call", &try_args(target, method, vec![], funds)).unwrap();
        assert_eq!(try_result(out), (2, Vec::new()), "{method}");
    }
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 100);

    // The caller's own errors cannot be caught: method name init, attached funds out of order
    assert!(
        trap_reason(call(a, "try_call", &try_args(b, "init", vec![], vec![]))).contains("init")
    );
    let unsorted = vec![
        Funds {
            asset: [9; 32],
            amount: 1,
        },
        ksa(1),
    ];
    assert!(call(a, "try_call", &try_args(b, "whoami", vec![], unsorted)).is_err());
}

#[test]
fn delegate_call_runs_code_in_the_current_context() {
    mock::reset();
    mock::register_asset(sdk::KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(sdk::KSA_ASSET_ID, ALICE, 1_000).unwrap();
    let a = deploy_probe(0);
    let b = deploy_probe(1);

    // The storage write goes under a's name
    let set = encode(&(b"k".to_vec(), b"v".to_vec()));
    call(a, "delegate", &encode(&(b, "set".to_string(), set))).unwrap();
    assert_eq!(mock::storage_value(&a, b"k"), Some(b"v".to_vec()));
    assert_eq!(mock::storage_value(&b, b"k"), None);

    // caller, self_id and the attached funds are kept from the current call; the funds are transferred only once (to a)
    let out = mock::call_with_funds(
        ALICE,
        a,
        "delegate",
        &encode(&(b, "whoami".to_string(), Vec::<u8>::new())),
        &[ksa(10)],
    )
    .unwrap();
    let data: Vec<u8> = decode(&out).unwrap();
    assert_eq!(decode::<([u8; 32], [u8; 32])>(&data).unwrap(), (ALICE, a));
    let out = mock::call_with_funds(
        ALICE,
        a,
        "delegate",
        &encode(&(b, "funds".to_string(), Vec::<u8>::new())),
        &[ksa(5)],
    )
    .unwrap();
    let data: Vec<u8> = decode(&out).unwrap();
    assert_eq!(received(data), (vec![ksa(5)], 5));
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 15);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &b), 0);

    // The delegated code transfers out a's balance
    let send = encode(&(sdk::KSA_ASSET_ID, BOB, 15u128));
    call(
        a,
        "delegate",
        &encode(&(b, "transfer_asset".to_string(), send)),
    )
    .unwrap();
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &BOB), 15);
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 0);

    // The delegated code fails: the whole operation fails
    assert!(matches!(
        call(
            a,
            "delegate",
            &encode(&(b, "revert".to_string(), Vec::<u8>::new()))
        ),
        Err(Failure::Revert { .. })
    ));
}

#[test]
fn environment_queries() {
    mock::reset();
    let a = deploy_probe(0);
    mock::set_block_timestamp(1_700_000_123);
    mock::set_gas_left(5_000);
    let out = call(a, "env", &[]).unwrap();
    assert_eq!(
        decode::<(u64, u32, u64)>(&out).unwrap(),
        (1_700_000_123, mock::DEFAULT_CHAIN_ID, 5_000)
    );
    mock::set_chain_id(1002);
    let out = call(a, "env", &[]).unwrap();
    assert_eq!(decode::<(u64, u32, u64)>(&out).unwrap().1, 1002);
}

#[test]
fn asset_burn_reduces_balance_and_supply() {
    mock::reset();
    mock::register_asset(sdk::KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(sdk::KSA_ASSET_ID, ALICE, 1_000).unwrap();
    let a = deploy_probe(0);
    mock::call_with_funds(
        ALICE,
        a,
        "burn",
        &encode(&(sdk::KSA_ASSET_ID, 30u128)),
        &[ksa(100)],
    )
    .unwrap();
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &a), 70);
    assert_eq!(
        mock::asset_info(&sdk::KSA_ASSET_ID).unwrap().total_supply,
        970
    );
    // Only the contract's own balance can be burned
    assert!(
        trap_reason(call(a, "burn", &encode(&(sdk::KSA_ASSET_ID, 71u128))))
            .contains("insufficient")
    );
    assert!(
        trap_reason(call(a, "burn", &encode(&([5u8; 32], 1u128)))).contains("asset does not exist")
    );
}

#[test]
fn contracts_create_contracts_from_registered_code() {
    mock::reset();
    mock::register_asset(sdk::KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(sdk::KSA_ASSET_ID, ALICE, 1_000).unwrap();
    let factory = deploy_probe(0);
    let code_hash = sdk::sha256(b"probe");
    let salt = [3u8; 32];
    let create =
        |tag: &[u8], funds: Vec<Funds>| encode(&(code_hash, salt, encode(&tag.to_vec()), funds));
    let out = mock::call_with_funds(
        ALICE,
        factory,
        "create",
        &create(b"ok", vec![ksa(9)]),
        &[ksa(9)],
    )
    .unwrap();
    let child: [u8; 32] = decode(&out).unwrap();
    assert_eq!(child, sdk::created_contract_id(&factory, &salt, &code_hash));
    assert!(mock::contract_exists(&child));
    assert_eq!(mock::balance(&sdk::KSA_ASSET_ID, &child), 9);
    // init runs with the creator as the caller
    assert_eq!(mock::storage_value(&child, b"init"), Some(b"ok".to_vec()));
    // The same address cannot be created again; unregistered code; the creation is undone when init fails
    assert!(trap_reason(call(factory, "create", &create(b"ok", vec![]))).contains("exists"));
    let unknown = encode(&(
        [8u8; 32],
        salt,
        encode(&b"ok".to_vec()),
        Vec::<Funds>::new(),
    ));
    assert!(trap_reason(call(factory, "create", &unknown)).contains("unknown code"));
    let other = encode(&(
        code_hash,
        [4u8; 32],
        encode(&b"fail".to_vec()),
        Vec::<Funds>::new(),
    ));
    assert!(call(factory, "create", &other).is_err());
    assert!(!mock::contract_exists(&sdk::created_contract_id(
        &factory, &[4u8; 32], &code_hash
    )));
    // Code registered with register_code is usable too
    let code_hash = mock::register_code(b"other code", probe);
    let out = call(
        factory,
        "create",
        &encode(&(
            code_hash,
            salt,
            encode(&b"ok".to_vec()),
            Vec::<Funds>::new(),
        )),
    )
    .unwrap();
    let second: [u8; 32] = decode(&out).unwrap();
    assert!(mock::contract_exists(&second));
}
