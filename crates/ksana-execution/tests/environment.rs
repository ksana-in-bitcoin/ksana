//! Execution environment and creation of assets and contracts (D-073): `block_timestamp`, `chain_id`, `gas_left`,
//! `asset_burn`, `create_contract`.

mod common;

use common::probe::*;
use common::*;
use ksana_execution::builtin::{contract_id, created_contract_id};
use ksana_execution::keys::{self, AssetMeta, ContractMeta};
use ksana_execution::OpStatus;
use ksana_primitives::hash::sha256;
use ksana_primitives::{Funds, DEVNET};
use ksana_test_contracts::PROBE;

fn supply(w: &World, asset: &[u8; 32]) -> u128 {
    w.chain
        .get::<AssetMeta>(&keys::asset(asset))
        .unwrap()
        .total_supply
}

#[test]
fn contracts_see_the_block_environment() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);
    let timestamp = w.chain.timestamp();
    let height = w.chain.height + 1;
    let r = w.run_gas(&alice, 1_000_000, call(p, "env", vec![], vec![]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let (h, t, chain, left): (u64, u64, u32, u64) = borsh::from_slice(&r.output).unwrap();
    assert_eq!((h, t, chain), (height, timestamp, DEVNET.chain_id));
    // Remaining gas: after deducting the intrinsic cost and the execution so far, it is below the limit and above the
    // part left over beyond this call's actual usage
    assert!(left < 1_000_000 - ksana_vm::gas::CALL_BASE, "{left}");
    assert!(
        left > 1_000_000 - r.gas_used,
        "{left} vs used {}",
        r.gas_used
    );
}

#[test]
fn asset_burn_reduces_balance_and_total_supply() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);
    let ksa_supply = supply(&w, &KSA);

    // Attach 100 KSA to the contract, which burns 30 of it
    let r = w.run(&alice, call(p, "burn", enc(&(KSA, 30u128)), vec![ksa(100)]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(w.chain.ksa(&p), 70);
    assert_eq!(supply(&w, &KSA), ksa_supply - 30);

    // Assets issued by a contract can be burned too
    let r = w.run(&alice, call(p, "issue", enc(&([1u8; 32], 500u128)), vec![]));
    let token: [u8; 32] = borsh::from_slice(&r.output).unwrap();
    let r = w.run(
        &alice,
        call(
            p,
            "burn",
            enc(&(token, 200u128)),
            vec![Funds {
                asset: token,
                amount: 200,
            }],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(supply(&w, &token), 300);
    assert_eq!(w.balance(&token, &p), 0);

    // Insufficient balance, asset does not exist: the op fails and changes nothing
    for (asset, amount) in [(KSA, 71u128), ([5u8; 32], 1)] {
        let r = w.run(&alice, call(p, "burn", enc(&(asset, amount)), vec![]));
        assert!(matches!(r.status, OpStatus::Failed(_)), "{r:?}");
    }
    assert_eq!(w.chain.ksa(&p), 70);
    assert_eq!(supply(&w, &KSA), ksa_supply - 30);
}

#[test]
fn contracts_create_contracts_from_deployed_code() {
    let alice = user("alice");
    let bob = user("bob");
    let mut w = World::new(&[&alice, &bob]);
    let factory = w.deploy(&alice, vec![]);
    let code_hash = sha256(PROBE);
    let create = |salt: [u8; 32], tag: &[u8], funds: Vec<Funds>| {
        enc(&(code_hash, salt, enc(&tag.to_vec()), funds))
    };

    // The address is sha256(creator ‖ salt ‖ code_hash); init runs with the factory as caller, the attached funds
    // arrive before init
    let r = w.run(
        &bob,
        call(
            factory,
            "create",
            create([1; 32], b"ok", vec![ksa(9)]),
            vec![ksa(9)],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let child: [u8; 32] = borsh::from_slice(&r.output).unwrap();
    assert_eq!(child, created_contract_id(&factory, &[1; 32], &code_hash));
    assert_ne!(child, contract_id(&factory, 0, &code_hash));
    let meta: ContractMeta = w.chain.get(&keys::contract(&child)).unwrap();
    assert_eq!((meta.code_hash, meta.deployer), (code_hash, factory));
    assert_eq!((w.chain.ksa(&factory), w.chain.ksa(&child)), (0, 9));
    let r = w.run(&bob, call(child, "init_funds", vec![], vec![]));
    assert_eq!(
        borsh::from_slice::<Vec<Funds>>(&r.output).unwrap(),
        vec![ksa(9)]
    );
    // The new contract can be called like any other contract
    let r = w.run(&bob, call(child, "whoami", vec![], vec![]));
    let (caller, me): ([u8; 32], [u8; 32]) = borsh::from_slice(&r.output).unwrap();
    assert_eq!((caller, me), (bob.account, child));

    // The new contract creates a contract in turn
    let r = w.run(
        &bob,
        call(child, "create", create([2; 32], b"ok", vec![]), vec![]),
    );
    let grandchild: [u8; 32] = borsh::from_slice(&r.output).unwrap();
    assert_eq!(
        grandchild,
        created_contract_id(&child, &[2; 32], &code_hash)
    );

    // Address already exists, code does not exist, init fails: the op fails and leaves no contract behind
    let missing_code = enc(&(
        [7u8; 32],
        [3u8; 32],
        enc(&b"ok".to_vec()),
        Vec::<Funds>::new(),
    ));
    for args in [
        create([1; 32], b"ok", vec![]),
        missing_code,
        create([4; 32], b"fail", vec![]),
    ] {
        let r = w.run(&bob, call(factory, "create", args, vec![]));
        assert!(matches!(r.status, OpStatus::Failed(_)), "{r:?}");
    }
    assert!(w
        .chain
        .get::<ContractMeta>(&keys::contract(&created_contract_id(
            &factory, &[4; 32], &code_hash
        )))
        .is_none());

    // Created inside a catchable sub-call that then fails: the creation is rolled back with it
    let inner = enc(&(
        factory,
        "create".to_string(),
        create([5; 32], b"ok", vec![]),
        Vec::<Funds>::new(),
    ));
    let r = w.run(
        &bob,
        call(
            factory,
            "try_forward",
            enc(&(
                child,
                "forward_then_revert".to_string(),
                inner,
                Vec::<Funds>::new(),
                0u64,
            )),
            vec![],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let (status, _): (u8, Vec<u8>) = borsh::from_slice(&r.output).unwrap();
    assert_eq!(status, 1);
    assert!(w
        .chain
        .get::<ContractMeta>(&keys::contract(&created_contract_id(
            &factory, &[5; 32], &code_hash
        )))
        .is_none());
}
