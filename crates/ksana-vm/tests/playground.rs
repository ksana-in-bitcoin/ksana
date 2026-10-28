//! The exact modules the contract playground ships (playground/src/examples/wasm, written by
//! playground/scripts/build-examples.mjs, D-119) pass the deployment checks and run in Ksana's real metered VM: every
//! example's module is valid bytecode, the counter answers as its ABI says, the mintable token issues its asset, and the
//! KS-20 token declares its standard and moves its balances.
mod common;

use common::{id, MockHost};
use ksana_primitives::{hash::sha256, DEVNET};
use ksana_vm::{CallOutcome, CallRequest, Vm};

const COUNTER: &[u8] =
    include_bytes!("../../../playground/src/examples/wasm/playground-counter.wasm");
const TOKEN: &[u8] = include_bytes!("../../../playground/src/examples/wasm/mintable-token.wasm");
const KS20: &[u8] = include_bytes!("../../../playground/src/examples/wasm/token.wasm");

const EXAMPLES: [(&str, &[u8]); 8] = [
    ("playground-counter", COUNTER),
    ("mintable-token", TOKEN),
    ("token", KS20),
    (
        "nft",
        include_bytes!("../../../playground/src/examples/wasm/nft.wasm"),
    ),
    (
        "amm",
        include_bytes!("../../../playground/src/examples/wasm/amm.wasm"),
    ),
    (
        "lending",
        include_bytes!("../../../playground/src/examples/wasm/lending.wasm"),
    ),
    (
        "multisig",
        include_bytes!("../../../playground/src/examples/wasm/multisig.wasm"),
    ),
    (
        "vesting",
        include_bytes!("../../../playground/src/examples/wasm/vesting.wasm"),
    ),
];

const GAS: u64 = 1_000_000;

/// Calls `method` of the contract `id(1)` as `id(2)` and returns the host and the output of a successful call.
fn call(vm: &Vm, host: MockHost, method: &str, args: Vec<u8>) -> (MockHost, Vec<u8>) {
    let (next, result) = vm.call(
        host,
        CallRequest {
            contract: id(1),
            caller: id(2),
            method: method.into(),
            args,
            funds: Vec::new(),
            allow_init: method == "init",
        },
        GAS,
    );
    match result.unwrap() {
        CallOutcome::Success { output, gas_used } => {
            assert!(gas_used > 0 && gas_used < GAS, "{method} used {gas_used}");
            (next, output)
        }
        other => panic!("unexpected {method} result: {other:?}"),
    }
}

#[test]
fn every_shipped_example_passes_the_deployment_checks() {
    let vm = Vm::new(&DEVNET).unwrap();
    for (name, code) in EXAMPLES {
        let hash = vm
            .validate_code(code)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(hash, sha256(code), "{name}");
    }
}

#[test]
fn the_shipped_counter_answers_as_its_abi_says() {
    let vm = Vm::new(&DEVNET).unwrap();
    let mut host = MockHost::default();
    host.deploy(id(1), COUNTER.to_vec());
    let count = |value: u64| value.to_le_bytes().to_vec();
    for (method, args, expected) in [
        ("init", Vec::new(), Vec::new()),
        ("get", Vec::new(), count(0)),
        ("increment", Vec::new(), count(1)),
        ("add", 5u64.to_le_bytes().to_vec(), count(6)),
        ("get", Vec::new(), count(6)),
    ] {
        let (next, output) = call(&vm, host, method, args);
        assert_eq!(output, expected, "{method}");
        host = next;
    }
    let changed = |value: u64| borsh::to_vec(&("changed", id(2), value)).unwrap();
    assert_eq!(host.events, vec![(id(1), changed(1)), (id(1), changed(6))]);
}

#[test]
fn the_shipped_token_issues_its_asset() {
    let vm = Vm::new(&DEVNET).unwrap();
    let mut host = MockHost::default();
    host.deploy(id(1), TOKEN.to_vec());
    let init = borsh::to_vec(&("Playground", "PLAY", 8u8, Some(1_000_000u128), 1_000u128)).unwrap();
    let (next, output) = call(&vm, host, "init", init);
    assert!(output.is_empty());
    host = next;
    let (next, output) = call(&vm, host, "mint", borsh::to_vec(&(id(3), 500u128)).unwrap());
    assert_eq!(output, 1_500u128.to_le_bytes());
    host = next;
    let assets: Vec<_> = host.assets.iter().collect();
    let [(asset, (meta, supply, issuer))] = assets[..] else {
        panic!("one asset issued: {:?}", host.assets.keys());
    };
    assert_eq!(
        (
            meta.name.as_str(),
            meta.symbol.as_str(),
            meta.decimals,
            *supply,
            *issuer
        ),
        ("Playground", "PLAY", 8, 1_500, id(1))
    );
    assert_eq!(host.balances.get(&(*asset, id(2))), Some(&1_000));
    assert_eq!(host.balances.get(&(*asset, id(3))), Some(&500));
}

#[test]
fn the_shipped_ks20_token_declares_its_standard_and_moves_balances() {
    let vm = Vm::new(&DEVNET).unwrap();
    let mut host = MockHost::default();
    host.deploy(id(1), KS20.to_vec());
    let init = borsh::to_vec(&(
        "Demo Token",
        "DEMO",
        8u8,
        1_000u128,
        Some(id(2)),
        Some(5_000u128),
    ))
    .unwrap();
    let (next, output) = call(&vm, host, "init", init);
    assert!(output.is_empty());
    host = next;
    let (next, output) = call(&vm, host, "standards", Vec::new());
    let standards: Vec<String> = borsh::from_slice(&output).unwrap();
    assert_eq!(standards, ["KS-20", "KS-20-burnable", "KS-20-mintable"]);
    host = next;
    let (next, _) = call(
        &vm,
        host,
        "transfer",
        borsh::to_vec(&(id(3), 300u128)).unwrap(),
    );
    host = next;
    for (account, balance) in [(id(2), 700u128), (id(3), 300)] {
        let (next, output) = call(&vm, host, "balance_of", borsh::to_vec(&account).unwrap());
        assert_eq!(output, balance.to_le_bytes());
        host = next;
    }
    let transfer = |from: [u8; 32], to: [u8; 32], amount: u128| {
        (
            id(1),
            borsh::to_vec(&("transfer", from, to, amount)).unwrap(),
        )
    };
    assert_eq!(
        host.events[host.events.len() - 2..],
        [transfer([0; 32], id(2), 1_000), transfer(id(2), id(3), 300)]
    );
}
