//! Tests of the multisig wallet on the SDK's in-memory mock host: the proposal flow of a 2-of-3 wallet, the owner
//! actions and the approvals they void, calls to other contracts with attached funds (a callee that fails, one that
//! calls back), the checks and access control of every method, paging, a random walk against a model that drops a
//! removed owner's approvals at once, and the ABI block against `dispatch` and the events.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{
    self as sdk, decode, encode, storage, CallError, CallResult, Funds, KSA_ASSET_ID,
};
use multisig::{
    Action, Proposal, Status, Wallet, MAX_ARGS_BYTES, MAX_METHOD_BYTES, MAX_OWNERS, MAX_PAGE_BYTES,
    MAX_PAGE_ITEMS,
};

const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];
const CAROL: [u8; 32] = [0xc4; 32];
const DAVE: [u8; 32] = [0xd4; 32];
/// Never an owner in the fixed tests.
const EVE: [u8; 32] = [0xe5; 32];
const FRANK: [u8; 32] = [0xf6; 32];
const KSA: [u8; 32] = KSA_ASSET_ID;
/// A second built-in asset; its ID is above KSA's, so `[ksa(..), usd(..)]` is in ascending order.
const USD: [u8; 32] = [0x05; 32];
/// The test contract the wallets call (see [`callee`]).
const CALLEE: [u8; 32] = [0xca; 32];
const SUPPLY: u128 = 1_000_000_000;
const CODE: &[u8] = b"multisig";
const T0: u64 = 1_760_000_000;

// ---------------------------------------------------------------------------
// Helpers

fn ksa(amount: u128) -> Funds {
    Funds { asset: KSA, amount }
}

fn usd(amount: u128) -> Funds {
    Funds { asset: USD, amount }
}

/// A fresh mock: KSA and USD, every test account holding `SUPPLY` of both, and the test callee.
fn fresh() {
    mock::reset();
    mock::set_block_timestamp(T0);
    mock::register_asset(KSA, "KSA", 8, [0; 32]);
    mock::register_asset(USD, "USD", 6, [0x99; 32]);
    for who in [ALICE, BOB, CAROL, DAVE, EVE, FRANK] {
        mock::mint(KSA, who, SUPPLY).unwrap();
        mock::mint(USD, who, SUPPLY).unwrap();
    }
    mock::register(CALLEE, callee);
}

/// The ID of the wallet ALICE deploys with `nonce`.
fn wallet_id(nonce: u64) -> [u8; 32] {
    sdk::contract_id(&ALICE, nonce, &sdk::sha256(CODE))
}

fn deploy(
    nonce: u64,
    owners: &[[u8; 32]],
    threshold: u8,
    funds: &[Funds],
) -> Result<[u8; 32], Failure> {
    mock::deploy_with_funds(
        ALICE,
        nonce,
        CODE,
        multisig::dispatch,
        &encode(&(owners.to_vec(), threshold)),
        funds,
    )
}

/// A fresh mock and a 2-of-3 wallet of ALICE, BOB and CAROL that holds nothing, with the event log cleared.
fn setup() -> [u8; 32] {
    fresh();
    let w = deploy(0, &[ALICE, BOB, CAROL], 2, &[]).unwrap();
    mock::clear_events();
    w
}

fn unit(result: Result<Vec<u8>, Failure>) -> Result<(), Failure> {
    result.map(|out| assert!(out.is_empty(), "a method returning () returns empty bytes"))
}

fn deposit(w: [u8; 32], who: [u8; 32], funds: &[Funds]) -> Result<(), Failure> {
    unit(mock::call_with_funds(who, w, "deposit", &[], funds))
}

fn propose(w: [u8; 32], who: [u8; 32], action: &Action) -> Result<u64, Failure> {
    mock::call(who, w, "propose", &encode(action)).map(|out| decode(&out).unwrap())
}

fn approve(w: [u8; 32], who: [u8; 32], id: u64) -> Result<(), Failure> {
    unit(mock::call(who, w, "approve", &encode(&id)))
}

fn revoke(w: [u8; 32], who: [u8; 32], id: u64) -> Result<(), Failure> {
    unit(mock::call(who, w, "revoke", &encode(&id)))
}

fn execute(w: [u8; 32], who: [u8; 32], id: u64) -> Result<Vec<u8>, Failure> {
    mock::call(who, w, "execute", &encode(&id)).map(|out| decode(&out).unwrap())
}

fn cancel(w: [u8; 32], who: [u8; 32], id: u64) -> Result<(), Failure> {
    unit(mock::call(who, w, "cancel", &encode(&id)))
}

fn wallet(w: [u8; 32]) -> Wallet {
    decode(&mock::call(EVE, w, "wallet", &[]).unwrap()).unwrap()
}

fn proposal(w: [u8; 32], id: u64) -> Option<Proposal> {
    decode(&mock::call(EVE, w, "proposal", &encode(&id)).unwrap()).unwrap()
}

fn page(w: [u8; 32], start: u64, limit: u32) -> Vec<Proposal> {
    decode(&mock::call(EVE, w, "proposals", &encode(&(start, limit))).unwrap()).unwrap()
}

/// The `balance` view, which must agree with the host's balance.
fn balance(w: [u8; 32], asset: [u8; 32]) -> u128 {
    let shown: u128 = decode(&mock::call(EVE, w, "balance", &encode(&asset)).unwrap()).unwrap();
    assert_eq!(shown, mock::balance(&asset, &w));
    shown
}

/// The reason of a revert by the wallet `w`.
fn reverted<T: Debug>(w: [u8; 32], result: Result<T, Failure>) -> String {
    match result {
        Err(Failure::Revert { contract, reason }) if contract == w => {
            String::from_utf8(reason).unwrap()
        }
        other => panic!("expected a revert by the wallet, got {other:?}"),
    }
}

fn transfer(asset: [u8; 32], to: [u8; 32], amount: u128) -> Action {
    Action::Transfer(asset, to, amount)
}

/// A `Call` of the test callee.
fn call_callee(method: &str, args: &[u8], funds: &[Funds]) -> Action {
    Action::Call(CALLEE, method.to_string(), args.to_vec(), funds.to_vec())
}

/// The events `contract` emitted since the event log was last cleared.
fn events_of(contract: [u8; 32]) -> Vec<Vec<u8>> {
    mock::events()
        .into_iter()
        .filter(|e| e.contract == contract)
        .map(|e| e.data)
        .collect()
}

/// An owner or an approval as stored: the account and its term.
type Member = ([u8; 32], u64);

/// A proposal's record as stored under `p ‖ id`: `(proposer, approvals, status, created_at)`.
type Record = ([u8; 32], Vec<Member>, u8, u64);

/// The approvals stored in a proposal's record, void ones included.
fn stored_approvals(w: [u8; 32], id: u64) -> Vec<Member> {
    let key = [b"p".as_slice(), id.to_be_bytes().as_slice()].concat();
    let (_, approvals, _, _): Record = decode(&mock::storage_value(&w, &key).unwrap()).unwrap();
    approvals
}

/// Overwrites the stored settings (`config`): `(owners, threshold, proposals, terms)`.
fn write_config(w: [u8; 32], owners: &[Member], threshold: u8, proposals: u64, terms: u64) {
    mock::with_context(ALICE, w, || {
        storage::write(b"config", &(owners.to_vec(), threshold, proposals, terms));
    });
}

/// The test contract the wallets call:
/// - `echo`: keeps the attached funds and returns its caller, those funds and its arguments;
/// - `fail`: reverts;
/// - `reenter(wallet, id)`: calls the wallet's `execute(id)`, so a revert there fails the whole operation;
/// - `try_reenter(wallet, id)`: the same with `try_call`, returning `(0, output)`, `(1, reason)` or `(2, [])`.
fn callee(method: &str, args: &[u8]) -> CallResult {
    match method {
        "echo" => sdk::ok(&(sdk::caller(), sdk::call_funds(), args.to_vec())),
        "fail" => sdk::revert("callee refuses"),
        "reenter" => {
            let (wallet, id): ([u8; 32], u64) = sdk::decode(args)?;
            Ok(sdk::call(&wallet, "execute", &encode(&id)))
        }
        "try_reenter" => {
            let (wallet, id): ([u8; 32], u64) = sdk::decode(args)?;
            sdk::ok(&match sdk::try_call(&wallet, "execute", &encode(&id)) {
                Ok(output) => (0u8, output),
                Err(CallError::Revert(reason)) => (1, reason),
                Err(CallError::Failed) => (2, Vec::new()),
            })
        }
        _ => sdk::revert("unknown method"),
    }
}

// ---------------------------------------------------------------------------
// init and deposit

#[test]
fn init_records_the_settings_and_keeps_attached_funds() {
    fresh();
    let w = deploy(0, &[ALICE, BOB, CAROL], 2, &[ksa(500), usd(7)]).unwrap();
    assert_eq!(w, wallet_id(0));
    assert_eq!(
        wallet(w),
        Wallet {
            owners: vec![ALICE, BOB, CAROL],
            threshold: 2,
            proposals: 0
        }
    );
    assert_eq!(balance(w, KSA), 500);
    assert_eq!(balance(w, USD), 7);
    assert_eq!(mock::balance(&KSA, &ALICE), SUPPLY - 500);
    assert_eq!(
        events_of(w),
        vec![
            encode(&("owner_added", ALICE)),
            encode(&("owner_added", BOB)),
            encode(&("owner_added", CAROL)),
            encode(&("threshold", 2u8)),
            encode(&("deposit", ALICE, KSA, 500u128)),
            encode(&("deposit", ALICE, USD, 7u128)),
        ]
    );
    assert_eq!(proposal(w, 1), None);
    assert!(page(w, 0, 50).is_empty());

    // The smallest wallet, and the largest with the highest threshold
    let single = deploy(1, &[EVE], 1, &[]).unwrap();
    assert_eq!(wallet(single).owners, vec![EVE]);
    let many: Vec<[u8; 32]> = (1..=20u8).map(|i| [i; 32]).collect();
    let largest = deploy(2, &many, 20, &[]).unwrap();
    assert_eq!(
        wallet(largest),
        Wallet {
            owners: many,
            threshold: 20,
            proposals: 0
        }
    );

    // init runs only at deployment
    assert!(matches!(
        mock::call(ALICE, w, "init", &encode(&(vec![BOB], 1u8))),
        Err(Failure::Trap(_))
    ));
    assert_eq!(
        mock::with_context(ALICE, w, || multisig::init(vec![BOB], 1)),
        Err(b"already initialized".to_vec())
    );
    assert_eq!(wallet(w).owners, vec![ALICE, BOB, CAROL]);
}

#[test]
fn init_refuses_bad_settings() {
    fresh();
    let w = wallet_id(0);
    let too_many: Vec<[u8; 32]> = (1..=21u8).map(|i| [i; 32]).collect();
    for (owners, threshold, reason) in [
        (vec![], 1u8, "owners must be 1 to 20"),
        (vec![], 0, "owners must be 1 to 20"),
        (too_many, 1, "owners must be 1 to 20"),
        (vec![ALICE, BOB, ALICE], 1, "duplicate owner"),
        (
            vec![ALICE, [0; 32]],
            1,
            "an owner must not be the zero account",
        ),
        (vec![ALICE, w], 1, "the wallet cannot own itself"),
        (
            vec![ALICE, BOB],
            0,
            "threshold must be 1 to the number of owners",
        ),
        (
            vec![ALICE, BOB],
            3,
            "threshold must be 1 to the number of owners",
        ),
        (
            vec![ALICE],
            u8::MAX,
            "threshold must be 1 to the number of owners",
        ),
    ] {
        let result = deploy(0, &owners, threshold, &[ksa(1)]);
        assert_eq!(reverted(w, result), reason, "{owners:?} {threshold}");
        assert!(!mock::contract_exists(&w));
    }
    for args in [
        encode(&vec![ALICE]),
        encode(&(vec![ALICE], 1u8, 0u8)),
        vec![],
    ] {
        let result = mock::deploy(ALICE, 0, CODE, multisig::dispatch, &args);
        assert_eq!(reverted(w, result), "malformed arguments");
    }
    // Nothing happened: the attached KSA came back and no event was kept
    assert_eq!(mock::balance(&KSA, &ALICE), SUPPLY);
    assert!(mock::events().is_empty());
}

#[test]
fn anyone_deposits_any_assets() {
    let w = setup();
    deposit(w, EVE, &[ksa(1_000), usd(20)]).unwrap();
    deposit(w, ALICE, &[usd(5)]).unwrap();
    assert_eq!(
        events_of(w),
        vec![
            encode(&("deposit", EVE, KSA, 1_000u128)),
            encode(&("deposit", EVE, USD, 20u128)),
            encode(&("deposit", ALICE, USD, 5u128)),
        ]
    );
    // Assets sent with the built-in AssetTransfer arrive without a call or an event; `balance` counts them too
    mock::mint(KSA, w, 3).unwrap();
    assert_eq!(balance(w, KSA), 1_003);
    assert_eq!(balance(w, USD), 25);
    assert_eq!(balance(w, [0x77; 32]), 0);
    assert_eq!(mock::balance(&KSA, &EVE), SUPPLY - 1_000);

    assert_eq!(
        reverted(w, deposit(w, EVE, &[])),
        "attach the assets to deposit"
    );
    assert_eq!(
        reverted(w, mock::call_with_funds(EVE, w, "deposit", &[0], &[ksa(1)])),
        "malformed arguments"
    );
    assert_eq!(mock::balance(&KSA, &EVE), SUPPLY - 1_000);
}

// ---------------------------------------------------------------------------
// The proposal flow

#[test]
fn two_of_three_transfer_end_to_end() {
    let w = setup();
    deposit(w, EVE, &[ksa(1_000)]).unwrap();
    mock::set_block_timestamp(T0 + 60);
    mock::clear_events();

    let id = propose(w, ALICE, &transfer(KSA, DAVE, 400)).unwrap();
    assert_eq!(id, 1);
    assert_eq!(
        proposal(w, id),
        Some(Proposal {
            id: 1,
            proposer: ALICE,
            action: transfer(KSA, DAVE, 400),
            approvals: vec![ALICE],
            status: Status::Pending,
            created_at: T0 + 60,
        })
    );
    assert_eq!(wallet(w).proposals, 1);

    // One approval of the two needed
    assert_eq!(reverted(w, execute(w, ALICE, id)), "not enough approvals");
    approve(w, BOB, id).unwrap();
    assert_eq!(proposal(w, id).unwrap().approvals, vec![ALICE, BOB]);

    // Any owner executes, CAROL that did not approve too
    assert_eq!(execute(w, CAROL, id).unwrap(), Vec::<u8>::new());
    assert_eq!(balance(w, KSA), 600);
    assert_eq!(mock::balance(&KSA, &DAVE), SUPPLY + 400);
    let executed = proposal(w, id).unwrap();
    assert_eq!(executed.status, Status::Executed);
    assert_eq!(executed.approvals, vec![ALICE, BOB]);
    assert_eq!(executed.created_at, T0 + 60);
    assert_eq!(
        events_of(w),
        vec![
            encode(&("propose", 1u64, ALICE)),
            encode(&("approve", 1u64, BOB)),
            encode(&("execute", 1u64, CAROL)),
        ]
    );

    // Executed once only
    for result in [
        execute(w, ALICE, id).map(drop),
        approve(w, CAROL, id),
        revoke(w, BOB, id),
        cancel(w, ALICE, id),
    ] {
        assert_eq!(reverted(w, result), "proposal is not pending");
    }
    assert_eq!(balance(w, KSA), 600);

    // The next proposal gets the next ID
    assert_eq!(propose(w, CAROL, &transfer(KSA, DAVE, 1)).unwrap(), 2);
}

#[test]
fn a_revoked_approval_no_longer_counts() {
    let w = setup();
    deposit(w, EVE, &[ksa(100)]).unwrap();
    let id = propose(w, ALICE, &transfer(KSA, EVE, 100)).unwrap();
    approve(w, BOB, id).unwrap();
    mock::clear_events();
    revoke(w, BOB, id).unwrap();
    assert_eq!(events_of(w), vec![encode(&("revoke", id, BOB))]);
    assert_eq!(proposal(w, id).unwrap().approvals, vec![ALICE]);
    assert_eq!(reverted(w, execute(w, ALICE, id)), "not enough approvals");
    assert_eq!(
        reverted(w, revoke(w, BOB, id)),
        "not approved by the caller"
    );
    assert_eq!(
        reverted(w, revoke(w, CAROL, id)),
        "not approved by the caller"
    );

    // The proposer can take back its own approval too, and approve again
    revoke(w, ALICE, id).unwrap();
    assert!(proposal(w, id).unwrap().approvals.is_empty());
    approve(w, CAROL, id).unwrap();
    approve(w, ALICE, id).unwrap();
    assert_eq!(proposal(w, id).unwrap().approvals, vec![CAROL, ALICE]);
    execute(w, BOB, id).unwrap();
    assert_eq!(balance(w, KSA), 0);
    assert_eq!(mock::balance(&KSA, &EVE), SUPPLY);
}

#[test]
fn approvals_are_once_per_owner_and_for_owners_only() {
    let w = setup();
    let id = propose(w, ALICE, &transfer(KSA, EVE, 1)).unwrap();
    // Proposing counts as the proposer's approval
    assert_eq!(reverted(w, approve(w, ALICE, id)), "already approved");
    approve(w, BOB, id).unwrap();
    assert_eq!(reverted(w, approve(w, BOB, id)), "already approved");
    assert_eq!(proposal(w, id).unwrap().approvals, vec![ALICE, BOB]);

    // Someone who is not an owner can only deposit and read
    mock::clear_events();
    for result in [
        propose(w, EVE, &transfer(KSA, EVE, 1)).map(drop),
        approve(w, EVE, id),
        revoke(w, EVE, id),
        execute(w, EVE, id).map(drop),
        cancel(w, EVE, id),
    ] {
        assert_eq!(reverted(w, result), "caller is not an owner");
    }
    // ... and so can a contract that is not one
    let as_contract = mock::call(CALLEE, w, "approve", &encode(&id));
    assert_eq!(reverted(w, as_contract), "caller is not an owner");
    assert!(mock::events().is_empty());
    assert_eq!(wallet(w).proposals, 1);
    assert_eq!(proposal(w, id).unwrap().approvals, vec![ALICE, BOB]);
}

#[test]
fn unknown_proposals_are_refused() {
    let w = setup();
    propose(w, ALICE, &transfer(KSA, EVE, 1)).unwrap();
    for id in [0u64, 2, u64::MAX] {
        for result in [
            approve(w, BOB, id),
            revoke(w, BOB, id),
            execute(w, BOB, id).map(drop),
            cancel(w, BOB, id),
        ] {
            assert_eq!(reverted(w, result), "unknown proposal");
        }
        assert_eq!(proposal(w, id), None);
    }
    for (method, args) in [
        ("approve", vec![1u8]),
        ("execute", encode(&1u32)),
        ("proposal", encode(&(1u64, 0u8))),
    ] {
        assert_eq!(
            reverted(w, mock::call(ALICE, w, method, &args)),
            "malformed arguments"
        );
    }
}

#[test]
fn only_the_proposer_cancels_a_pending_proposal() {
    let w = setup();
    let id = propose(w, ALICE, &transfer(KSA, EVE, 1)).unwrap();
    approve(w, BOB, id).unwrap();
    assert_eq!(
        reverted(w, cancel(w, BOB, id)),
        "only the proposer can cancel"
    );
    assert_eq!(reverted(w, cancel(w, EVE, id)), "caller is not an owner");
    mock::clear_events();
    cancel(w, ALICE, id).unwrap();
    assert_eq!(events_of(w), vec![encode(&("cancel", id))]);
    let cancelled = proposal(w, id).unwrap();
    assert_eq!(cancelled.status, Status::Cancelled);
    assert_eq!(cancelled.approvals, vec![ALICE, BOB]);
    for result in [
        execute(w, ALICE, id).map(drop),
        approve(w, CAROL, id),
        revoke(w, BOB, id),
        cancel(w, ALICE, id),
    ] {
        assert_eq!(reverted(w, result), "proposal is not pending");
    }

    // A proposer that is no longer an owner cannot cancel, and its own approval no longer counts; the owners can
    // still approve and execute its proposal
    deposit(w, EVE, &[ksa(10)]).unwrap();
    let pay = propose(w, CAROL, &transfer(KSA, DAVE, 10)).unwrap();
    approve(w, BOB, pay).unwrap();
    let remove = propose(w, ALICE, &Action::RemoveOwner(CAROL)).unwrap();
    approve(w, BOB, remove).unwrap();
    execute(w, ALICE, remove).unwrap();
    assert_eq!(reverted(w, cancel(w, CAROL, pay)), "caller is not an owner");
    assert_eq!(proposal(w, pay).unwrap().approvals, vec![BOB]);
    assert_eq!(reverted(w, execute(w, BOB, pay)), "not enough approvals");
    approve(w, ALICE, pay).unwrap();
    execute(w, BOB, pay).unwrap();
    assert_eq!(mock::balance(&KSA, &DAVE), SUPPLY + 10);
    assert_eq!(proposal(w, pay).unwrap().proposer, CAROL);
}

// ---------------------------------------------------------------------------
// Execution

#[test]
fn execution_checks_the_balance_of_the_moment() {
    let w = setup();
    let id = propose(w, ALICE, &transfer(USD, EVE, 300)).unwrap();
    approve(w, BOB, id).unwrap();
    assert_eq!(reverted(w, execute(w, ALICE, id)), "insufficient balance");
    deposit(w, BOB, &[usd(299)]).unwrap();
    assert_eq!(reverted(w, execute(w, ALICE, id)), "insufficient balance");
    assert_eq!(proposal(w, id).unwrap().status, Status::Pending);
    // Exactly the balance
    deposit(w, BOB, &[usd(1)]).unwrap();
    execute(w, ALICE, id).unwrap();
    assert_eq!(balance(w, USD), 0);
    assert_eq!(mock::balance(&USD, &EVE), SUPPLY + 300);

    // Each of a Call's funds must be covered
    deposit(w, BOB, &[ksa(10), usd(4)]).unwrap();
    let id = propose(w, ALICE, &call_callee("echo", &[], &[ksa(10), usd(5)])).unwrap();
    approve(w, CAROL, id).unwrap();
    assert_eq!(reverted(w, execute(w, ALICE, id)), "insufficient balance");
    deposit(w, BOB, &[usd(1)]).unwrap();
    execute(w, ALICE, id).unwrap();
    assert_eq!((balance(w, KSA), balance(w, USD)), (0, 0));

    // An asset nobody issued is just a balance of 0
    let id = propose(w, ALICE, &transfer([0x77; 32], EVE, 1)).unwrap();
    approve(w, CAROL, id).unwrap();
    assert_eq!(reverted(w, execute(w, ALICE, id)), "insufficient balance");
}

#[test]
fn a_call_attaches_funds_and_returns_the_output() {
    let w = setup();
    deposit(w, EVE, &[ksa(1_000), usd(50)]).unwrap();
    let args = encode(&("hello", 42u32));
    let id = propose(w, BOB, &call_callee("echo", &args, &[ksa(400), usd(50)])).unwrap();
    approve(w, CAROL, id).unwrap();
    mock::clear_events();
    let output = execute(w, ALICE, id).unwrap();
    let (caller, funds, echoed): ([u8; 32], Vec<Funds>, Vec<u8>) = decode(&output).unwrap();
    assert_eq!(caller, w);
    assert_eq!(funds, vec![ksa(400), usd(50)]);
    assert_eq!(echoed, args);
    assert_eq!((balance(w, KSA), balance(w, USD)), (600, 0));
    assert_eq!(
        (mock::balance(&KSA, &CALLEE), mock::balance(&USD, &CALLEE)),
        (400, 50)
    );
    assert_eq!(events_of(w), vec![encode(&("execute", id, ALICE))]);
    assert_eq!(proposal(w, id).unwrap().status, Status::Executed);

    // Without funds
    let id = propose(w, BOB, &call_callee("echo", &[], &[])).unwrap();
    approve(w, ALICE, id).unwrap();
    let (caller, funds, echoed): ([u8; 32], Vec<Funds>, Vec<u8>) =
        decode(&execute(w, ALICE, id).unwrap()).unwrap();
    assert_eq!(caller, w);
    assert!(funds.is_empty() && echoed.is_empty());
    assert_eq!(balance(w, KSA), 600);
}

#[test]
fn a_failing_callee_fails_the_operation_and_the_proposal_stays_pending() {
    let w = setup();
    deposit(w, EVE, &[ksa(1_000)]).unwrap();
    let id = propose(w, ALICE, &call_callee("fail", &[], &[ksa(100)])).unwrap();
    approve(w, BOB, id).unwrap();
    mock::clear_events();
    assert_eq!(
        execute(w, CAROL, id),
        Err(Failure::Revert {
            contract: CALLEE,
            reason: b"callee refuses".to_vec()
        })
    );
    let pending = proposal(w, id).unwrap();
    assert_eq!(pending.status, Status::Pending);
    assert_eq!(pending.approvals, vec![ALICE, BOB]);
    assert_eq!(balance(w, KSA), 1_000);
    assert_eq!(mock::balance(&KSA, &CALLEE), 0);
    assert!(mock::events().is_empty());

    // A contract that does not exist: the call traps, with the same outcome
    let missing = Action::Call([0xee; 32], "anything".to_string(), vec![], vec![ksa(1)]);
    let other = propose(w, ALICE, &missing).unwrap();
    approve(w, BOB, other).unwrap();
    assert!(matches!(execute(w, ALICE, other), Err(Failure::Trap(_))));
    assert_eq!(proposal(w, other).unwrap().status, Status::Pending);
    assert_eq!(balance(w, KSA), 1_000);

    // Such a proposal can be executed again (it fails again) or cancelled by its proposer
    assert!(execute(w, BOB, id).is_err());
    cancel(w, ALICE, id).unwrap();
    assert_eq!(proposal(w, id).unwrap().status, Status::Cancelled);
}

#[test]
fn a_callee_calling_back_cannot_execute_the_proposal_again() {
    fresh();
    // The callee is an owner, so that nothing but the proposal's status stops its second execution
    let w = deploy(0, &[ALICE, BOB, CALLEE], 2, &[ksa(1_000)]).unwrap();
    let reenter = call_callee("reenter", &encode(&(w, 1u64)), &[ksa(100)]);
    assert_eq!(propose(w, ALICE, &reenter).unwrap(), 1);
    approve(w, BOB, 1).unwrap();
    assert_eq!(
        execute(w, ALICE, 1),
        Err(Failure::Revert {
            contract: w,
            reason: b"proposal is not pending".to_vec()
        })
    );
    assert_eq!(proposal(w, 1).unwrap().status, Status::Pending);
    assert_eq!(balance(w, KSA), 1_000);

    // Caught with try_call, the second execution is refused and the first completes: the funds move once
    let try_reenter = call_callee("try_reenter", &encode(&(w, 2u64)), &[ksa(100)]);
    assert_eq!(propose(w, ALICE, &try_reenter).unwrap(), 2);
    approve(w, BOB, 2).unwrap();
    let output: (u8, Vec<u8>) = decode(&execute(w, ALICE, 2).unwrap()).unwrap();
    assert_eq!(output, (1, b"proposal is not pending".to_vec()));
    assert_eq!(proposal(w, 2).unwrap().status, Status::Executed);
    assert_eq!(balance(w, KSA), 900);
    assert_eq!(mock::balance(&KSA, &CALLEE), 100);
}

#[test]
fn a_wallet_can_be_an_owner_of_another() {
    fresh();
    let outer = deploy(0, &[ALICE, BOB, CAROL], 2, &[]).unwrap();
    let inner = deploy(1, &[outer, DAVE], 2, &[usd(70)]).unwrap();
    // The outer wallet proposes on the inner one through a Call
    let pay = transfer(USD, EVE, 70);
    let id = propose(
        outer,
        ALICE,
        &Action::Call(inner, "propose".to_string(), encode(&pay), vec![]),
    )
    .unwrap();
    approve(outer, BOB, id).unwrap();
    let inner_id: u64 = decode(&execute(outer, CAROL, id).unwrap()).unwrap();
    assert_eq!(inner_id, 1);
    let inner_proposal = proposal(inner, inner_id).unwrap();
    assert_eq!(inner_proposal.proposer, outer);
    assert_eq!(inner_proposal.approvals, vec![outer]);
    approve(inner, DAVE, inner_id).unwrap();
    execute(inner, DAVE, inner_id).unwrap();
    assert_eq!(balance(inner, USD), 0);
    assert_eq!(mock::balance(&USD, &EVE), SUPPLY + 70);
}

// ---------------------------------------------------------------------------
// Owner actions

#[test]
fn owners_and_threshold_change_through_proposals() {
    let w = setup();
    let add = propose(w, ALICE, &Action::AddOwner(DAVE)).unwrap();
    approve(w, BOB, add).unwrap();
    assert_eq!(execute(w, CAROL, add).unwrap(), Vec::<u8>::new());
    assert_eq!(
        wallet(w),
        Wallet {
            owners: vec![ALICE, BOB, CAROL, DAVE],
            threshold: 2,
            proposals: 1
        }
    );
    // DAVE acts as an owner at once
    let raise = propose(w, DAVE, &Action::SetThreshold(3)).unwrap();
    approve(w, ALICE, raise).unwrap();
    execute(w, DAVE, raise).unwrap();
    assert_eq!(wallet(w).threshold, 3);
    // ALICE approves her own removal
    let remove = propose(w, BOB, &Action::RemoveOwner(ALICE)).unwrap();
    approve(w, CAROL, remove).unwrap();
    assert_eq!(reverted(w, execute(w, BOB, remove)), "not enough approvals");
    approve(w, ALICE, remove).unwrap();
    execute(w, ALICE, remove).unwrap();
    assert_eq!(
        wallet(w),
        Wallet {
            owners: vec![BOB, CAROL, DAVE],
            threshold: 3,
            proposals: 3
        }
    );
    assert_eq!(
        reverted(w, propose(w, ALICE, &transfer(KSA, ALICE, 1))),
        "caller is not an owner"
    );
    // The executed removal keeps the approvals that counted then, ALICE's included
    assert_eq!(
        proposal(w, remove).unwrap().approvals,
        vec![BOB, CAROL, ALICE]
    );
    assert_eq!(
        events_of(w),
        vec![
            encode(&("propose", 1u64, ALICE)),
            encode(&("approve", 1u64, BOB)),
            encode(&("execute", 1u64, CAROL)),
            encode(&("owner_added", DAVE)),
            encode(&("propose", 2u64, DAVE)),
            encode(&("approve", 2u64, ALICE)),
            encode(&("execute", 2u64, DAVE)),
            encode(&("threshold", 3u8)),
            encode(&("propose", 3u64, BOB)),
            encode(&("approve", 3u64, CAROL)),
            encode(&("approve", 3u64, ALICE)),
            encode(&("execute", 3u64, ALICE)),
            encode(&("owner_removed", ALICE)),
        ]
    );
    // Lowered to 1, a single owner proposes and executes
    let lower = propose(w, BOB, &Action::SetThreshold(1)).unwrap();
    approve(w, CAROL, lower).unwrap();
    approve(w, DAVE, lower).unwrap();
    execute(w, BOB, lower).unwrap();
    let solo = propose(w, DAVE, &Action::RemoveOwner(BOB)).unwrap();
    execute(w, DAVE, solo).unwrap();
    assert_eq!(wallet(w).owners, vec![CAROL, DAVE]);
}

#[test]
fn approvals_of_a_removed_owner_stop_counting() {
    let w = setup();
    deposit(w, EVE, &[ksa(100)]).unwrap();
    let pay = propose(w, ALICE, &transfer(KSA, EVE, 100)).unwrap();
    approve(w, BOB, pay).unwrap();
    let remove = propose(w, CAROL, &Action::RemoveOwner(BOB)).unwrap();
    approve(w, ALICE, remove).unwrap();
    execute(w, CAROL, remove).unwrap();
    // BOB's approval no longer counts, though nothing rewrote the proposal
    assert_eq!(stored_approvals(w, pay), vec![(ALICE, 0), (BOB, 0)]);
    assert_eq!(proposal(w, pay).unwrap().approvals, vec![ALICE]);
    assert_eq!(reverted(w, execute(w, ALICE, pay)), "not enough approvals");

    // Added back, BOB is in a new term: the old approval stays void until BOB approves again
    let add = propose(w, ALICE, &Action::AddOwner(BOB)).unwrap();
    approve(w, CAROL, add).unwrap();
    execute(w, ALICE, add).unwrap();
    assert_eq!(wallet(w).owners, vec![ALICE, CAROL, BOB]);
    assert_eq!(proposal(w, pay).unwrap().approvals, vec![ALICE]);
    assert_eq!(reverted(w, execute(w, ALICE, pay)), "not enough approvals");
    assert_eq!(
        reverted(w, revoke(w, BOB, pay)),
        "not approved by the caller"
    );
    approve(w, BOB, pay).unwrap();
    // The approval rewrote the record without the void one
    assert_eq!(stored_approvals(w, pay), vec![(ALICE, 0), (BOB, 1)]);
    assert_eq!(proposal(w, pay).unwrap().approvals, vec![ALICE, BOB]);
    execute(w, CAROL, pay).unwrap();
    assert_eq!(balance(w, KSA), 0);
}

#[test]
fn a_removal_cannot_leave_fewer_owners_than_the_threshold() {
    fresh();
    let pair = deploy(0, &[ALICE, BOB], 2, &[]).unwrap();
    assert_eq!(
        reverted(pair, propose(pair, ALICE, &Action::RemoveOwner(BOB))),
        "removal would leave fewer owners than the threshold"
    );
    let single = deploy(1, &[ALICE], 1, &[]).unwrap();
    assert_eq!(
        reverted(single, propose(single, ALICE, &Action::RemoveOwner(ALICE))),
        "removal would leave fewer owners than the threshold"
    );

    // Checked again at execution: the threshold rose in between
    let w = deploy(2, &[ALICE, BOB, CAROL], 2, &[]).unwrap();
    let remove = propose(w, ALICE, &Action::RemoveOwner(CAROL)).unwrap();
    approve(w, BOB, remove).unwrap();
    let raise = propose(w, ALICE, &Action::SetThreshold(3)).unwrap();
    approve(w, BOB, raise).unwrap();
    execute(w, ALICE, raise).unwrap();
    approve(w, CAROL, remove).unwrap();
    assert_eq!(
        reverted(w, execute(w, ALICE, remove)),
        "removal would leave fewer owners than the threshold"
    );
    assert_eq!(wallet(w).owners, vec![ALICE, BOB, CAROL]);
    assert_eq!(proposal(w, remove).unwrap().status, Status::Pending);
}

#[test]
fn owner_actions_are_checked_when_proposed_and_again_when_executed() {
    let w = setup();
    for (action, reason) in [
        (
            Action::AddOwner([0; 32]),
            "an owner must not be the zero account",
        ),
        (Action::AddOwner(w), "the wallet cannot own itself"),
        (Action::AddOwner(BOB), "already an owner"),
        (Action::RemoveOwner(EVE), "account is not an owner"),
        (Action::RemoveOwner(w), "account is not an owner"),
        (
            Action::SetThreshold(0),
            "threshold must be 1 to the number of owners",
        ),
        (
            Action::SetThreshold(4),
            "threshold must be 1 to the number of owners",
        ),
    ] {
        assert_eq!(
            reverted(w, propose(w, ALICE, &action)),
            reason,
            "{action:?}"
        );
    }
    assert_eq!(wallet(w).proposals, 0);

    // Two proposals to add DAVE, two to remove CAROL, and a threshold of 3 for three owners
    let add = propose(w, ALICE, &Action::AddOwner(DAVE)).unwrap();
    let add_again = propose(w, BOB, &Action::AddOwner(DAVE)).unwrap();
    let remove = propose(w, ALICE, &Action::RemoveOwner(CAROL)).unwrap();
    let remove_again = propose(w, BOB, &Action::RemoveOwner(CAROL)).unwrap();
    let three = propose(w, ALICE, &Action::SetThreshold(3)).unwrap();
    for id in [add, remove] {
        approve(w, BOB, id).unwrap();
    }
    for id in [add_again, remove_again] {
        approve(w, ALICE, id).unwrap();
    }
    approve(w, CAROL, three).unwrap();
    execute(w, ALICE, add).unwrap();
    assert_eq!(
        reverted(w, execute(w, ALICE, add_again)),
        "already an owner"
    );
    execute(w, ALICE, remove).unwrap();
    assert_eq!(
        reverted(w, execute(w, ALICE, remove_again)),
        "account is not an owner"
    );
    // CAROL's approvals went with her: the threshold proposal needs another
    assert_eq!(
        reverted(w, execute(w, ALICE, three)),
        "not enough approvals"
    );
    approve(w, DAVE, three).unwrap();
    execute(w, ALICE, three).unwrap();
    assert_eq!(
        wallet(w),
        Wallet {
            owners: vec![ALICE, BOB, DAVE],
            threshold: 3,
            proposals: 5
        }
    );

    // A threshold proposed for more owners than remain when it is executed
    let four = propose(w, ALICE, &Action::AddOwner(EVE)).unwrap();
    approve(w, BOB, four).unwrap();
    approve(w, DAVE, four).unwrap();
    execute(w, ALICE, four).unwrap();
    let to_four = propose(w, ALICE, &Action::SetThreshold(4)).unwrap();
    let lower = propose(w, ALICE, &Action::SetThreshold(2)).unwrap();
    let drop_eve = propose(w, ALICE, &Action::RemoveOwner(EVE)).unwrap();
    for id in [to_four, lower, drop_eve] {
        approve(w, BOB, id).unwrap();
        approve(w, DAVE, id).unwrap();
    }
    execute(w, ALICE, lower).unwrap();
    execute(w, ALICE, drop_eve).unwrap();
    assert_eq!(
        reverted(w, execute(w, ALICE, to_four)),
        "threshold must be 1 to the number of owners"
    );
}

#[test]
fn twenty_owners_at_most() {
    fresh();
    let nineteen: Vec<[u8; 32]> = (1..=19u8).map(|i| [i; 32]).collect();
    let w = deploy(0, &nineteen, 1, &[]).unwrap();
    let first = propose(w, nineteen[0], &Action::AddOwner(EVE)).unwrap();
    let second = propose(w, nineteen[0], &Action::AddOwner(FRANK)).unwrap();
    execute(w, nineteen[1], first).unwrap();
    assert_eq!(wallet(w).owners.len(), MAX_OWNERS);
    assert_eq!(
        reverted(w, execute(w, nineteen[1], second)),
        "at most 20 owners"
    );
    assert_eq!(
        reverted(w, propose(w, EVE, &Action::AddOwner(FRANK))),
        "at most 20 owners"
    );
}

#[test]
fn a_proposal_holds_at_most_twenty_approvals() {
    fresh();
    let owners: Vec<[u8; 32]> = (1..=20u8).map(|i| [i; 32]).collect();
    let w = deploy(0, &owners, 19, &[]).unwrap();
    // All twenty approve the first proposal
    let first = propose(w, owners[0], &Action::SetThreshold(19)).unwrap();
    for owner in &owners[1..] {
        approve(w, *owner, first).unwrap();
    }
    assert_eq!(proposal(w, first).unwrap().approvals, owners);
    // The last owner leaves, then a newcomer joins: nineteen approvals each
    let newcomer = [0x55; 32];
    let remove = propose(w, owners[0], &Action::RemoveOwner(owners[19])).unwrap();
    for owner in &owners[1..19] {
        approve(w, *owner, remove).unwrap();
    }
    execute(w, owners[1], remove).unwrap();
    let add = propose(w, owners[0], &Action::AddOwner(newcomer)).unwrap();
    for owner in &owners[1..19] {
        approve(w, *owner, add).unwrap();
    }
    execute(w, owners[1], add).unwrap();
    assert_eq!(wallet(w).owners.len(), 20);
    // The departed owner's approval is still stored until the next write, which drops it
    assert_eq!(stored_approvals(w, first).len(), 20);
    assert_eq!(proposal(w, first).unwrap().approvals.len(), 19);
    approve(w, newcomer, first).unwrap();
    let stored = stored_approvals(w, first);
    assert_eq!(stored.len(), 20);
    assert!(stored.iter().all(|(account, _)| *account != owners[19]));
    assert_eq!(stored.last(), Some(&(newcomer, 1)));
    let mut shown = owners[..19].to_vec();
    shown.push(newcomer);
    assert_eq!(proposal(w, first).unwrap().approvals, shown);
    execute(w, newcomer, first).unwrap();
}

// ---------------------------------------------------------------------------
// The shape of actions, attached funds, initialization

#[test]
fn propose_checks_the_shape_of_the_action() {
    let w = setup();
    let assets = |n: u8| -> Vec<Funds> {
        (1..=n)
            .map(|i| Funds {
                asset: [i; 32],
                amount: 1,
            })
            .collect()
    };
    let call = |contract: [u8; 32], method: &str, args: Vec<u8>, funds: Vec<Funds>| {
        Action::Call(contract, method.to_string(), args, funds)
    };
    let longest = "m".repeat(MAX_METHOD_BYTES);
    let method = "method must be 1 to 64 bytes and not init";
    let order = "funds must be sorted by asset without duplicates";
    for (action, reason) in [
        (
            transfer(KSA, [0; 32], 1),
            "recipient must not be the zero account",
        ),
        (transfer(KSA, EVE, 0), "amount must be positive"),
        (
            call(w, "deposit", vec![], vec![]),
            "use the owner actions to change the wallet",
        ),
        (
            call(w, "propose", vec![], vec![ksa(1)]),
            "use the owner actions to change the wallet",
        ),
        (
            call([0; 32], "x", vec![], vec![]),
            "contract must not be the zero account",
        ),
        (call(CALLEE, "", vec![], vec![]), method),
        (call(CALLEE, &format!("{longest}m"), vec![], vec![]), method),
        (call(CALLEE, "init", vec![], vec![]), method),
        (
            call(CALLEE, "x", vec![0; MAX_ARGS_BYTES + 1], vec![]),
            "args must be at most 16 KiB",
        ),
        (call(CALLEE, "x", vec![], assets(17)), "at most 16 funds"),
        (call(CALLEE, "x", vec![], vec![usd(1), ksa(1)]), order),
        (call(CALLEE, "x", vec![], vec![ksa(1), ksa(2)]), order),
        (
            call(CALLEE, "x", vec![], vec![ksa(0)]),
            "funds amounts must be positive",
        ),
        (
            call(CALLEE, "x", vec![], vec![ksa(1), usd(0)]),
            "funds amounts must be positive",
        ),
    ] {
        assert_eq!(
            reverted(w, propose(w, ALICE, &action)),
            reason,
            "{action:?}"
        );
    }
    assert_eq!(wallet(w).proposals, 0);

    // The limits themselves are accepted
    for action in [
        transfer(KSA, EVE, 1),
        transfer(KSA, EVE, u128::MAX),
        call(CALLEE, &longest, vec![7; MAX_ARGS_BYTES], assets(16)),
        call(CALLEE, "init_later", vec![], vec![ksa(u128::MAX), usd(1)]),
    ] {
        let id = propose(w, ALICE, &action).unwrap();
        assert_eq!(proposal(w, id).unwrap().action, action);
    }

    // Undecodable actions
    let mut trailing = encode(&Action::SetThreshold(1));
    trailing.push(0);
    for args in [vec![], vec![5u8], vec![4u8], trailing] {
        assert_eq!(
            reverted(w, mock::call(ALICE, w, "propose", &args)),
            "malformed arguments"
        );
    }
    assert_eq!(wallet(w).proposals, 4);
}

#[test]
fn only_init_and_deposit_take_attached_funds() {
    let w = setup();
    let id = propose(w, ALICE, &transfer(KSA, EVE, 1)).unwrap();
    approve(w, BOB, id).unwrap();
    mock::clear_events();
    for (method, args) in [
        ("propose", encode(&transfer(KSA, EVE, 1))),
        ("approve", encode(&id)),
        ("revoke", encode(&id)),
        ("execute", encode(&id)),
        ("cancel", encode(&id)),
        ("wallet", vec![]),
        ("proposal", encode(&id)),
        ("proposals", encode(&(0u64, 10u32))),
        ("balance", encode(&KSA)),
    ] {
        let result = mock::call_with_funds(ALICE, w, method, &args, &[ksa(5)]);
        assert_eq!(reverted(w, result), "unexpected attached funds", "{method}");
    }
    assert_eq!(mock::balance(&KSA, &ALICE), SUPPLY);
    assert_eq!(balance(w, KSA), 0);
    assert!(mock::events().is_empty());
    assert_eq!(proposal(w, id).unwrap().status, Status::Pending);
}

#[test]
fn every_method_waits_for_init() {
    fresh();
    // The code registered without running init
    let w = [0x44; 32];
    mock::register(w, multisig::dispatch);
    for (method, args, funds) in [
        ("deposit", vec![], vec![ksa(1)]),
        ("propose", encode(&transfer(KSA, EVE, 1)), vec![]),
        ("approve", encode(&1u64), vec![]),
        ("revoke", encode(&1u64), vec![]),
        ("execute", encode(&1u64), vec![]),
        ("cancel", encode(&1u64), vec![]),
        ("wallet", vec![], vec![]),
        ("proposal", encode(&1u64), vec![]),
        ("proposals", encode(&(0u64, 10u32)), vec![]),
        ("balance", encode(&KSA), vec![]),
    ] {
        let result = mock::call_with_funds(ALICE, w, method, &args, &funds);
        assert_eq!(reverted(w, result), "not initialized", "{method}");
    }
}

// ---------------------------------------------------------------------------
// Views

#[test]
fn proposals_are_paged_by_id() {
    let w = setup();
    for amount in 1..=60u128 {
        propose(w, ALICE, &transfer(KSA, EVE, amount)).unwrap();
    }
    let ids = |page: &[Proposal]| page.iter().map(|p| p.id).collect::<Vec<_>>();
    assert_eq!(ids(&page(w, 0, 10)), (1..=10).collect::<Vec<_>>());
    assert_eq!(page(w, 1, 10), page(w, 0, 10));
    assert_eq!(ids(&page(w, 55, 10)), (55..=60).collect::<Vec<_>>());
    assert_eq!(ids(&page(w, 60, 1)), vec![60]);
    assert!(page(w, 61, 10).is_empty());
    assert!(page(w, u64::MAX, u32::MAX).is_empty());
    assert!(page(w, 0, 0).is_empty());
    // At most 50 per page
    let max = usize::try_from(MAX_PAGE_ITEMS).unwrap();
    assert_eq!(ids(&page(w, 0, u32::MAX)), (1..=50).collect::<Vec<_>>());
    assert_eq!(page(w, 0, 51).len(), max);
    assert_eq!(ids(&page(w, 51, 100)), (51..=60).collect::<Vec<_>>());
    // The items are the proposals as `proposal` shows them
    approve(w, BOB, 7).unwrap();
    let seventh = page(w, 7, 1);
    assert_eq!(seventh, vec![proposal(w, 7).unwrap()]);
    assert_eq!(seventh[0].action, transfer(KSA, EVE, 7));
    assert_eq!(seventh[0].approvals, vec![ALICE, BOB]);
    assert_eq!(
        reverted(w, mock::call(EVE, w, "proposals", &encode(&5u64))),
        "malformed arguments"
    );
}

#[test]
fn a_page_stays_within_the_output_limit() {
    let w = setup();
    let funds: Vec<Funds> = (1..=16u8)
        .map(|i| Funds {
            asset: [i; 32],
            amount: u128::MAX,
        })
        .collect();
    let largest = Action::Call(
        CALLEE,
        "m".repeat(MAX_METHOD_BYTES),
        vec![0xab; MAX_ARGS_BYTES],
        funds,
    );
    for _ in 0..5 {
        propose(w, ALICE, &largest).unwrap();
    }
    for id in 1..=5 {
        approve(w, BOB, id).unwrap();
        approve(w, CAROL, id).unwrap();
    }
    let size = encode(&proposal(w, 1).unwrap()).len();
    assert!(
        size * 3 <= MAX_PAGE_BYTES && size * 4 > MAX_PAGE_BYTES,
        "{size}"
    );
    // Four of them would not fit in a call's 64 KiB output: the page stops at three
    let first = page(w, 0, 50);
    assert_eq!(
        first.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let rest = page(w, 4, 50);
    assert_eq!(rest.iter().map(|p| p.id).collect::<Vec<_>>(), vec![4, 5]);
    assert_eq!(rest[1], proposal(w, 5).unwrap());
}

#[test]
fn views_only_read() {
    let w = setup();
    deposit(w, EVE, &[ksa(5)]).unwrap();
    let pay = propose(w, ALICE, &transfer(KSA, EVE, 5)).unwrap();
    approve(w, BOB, pay).unwrap();
    let remove = propose(w, CAROL, &Action::RemoveOwner(BOB)).unwrap();
    approve(w, ALICE, remove).unwrap();
    execute(w, CAROL, remove).unwrap();
    let config = mock::storage_value(&w, b"config");
    let stored = stored_approvals(w, pay);
    mock::clear_events();
    for (method, args) in [
        ("wallet", vec![]),
        ("proposal", encode(&pay)),
        ("proposals", encode(&(0u64, 50u32))),
        ("balance", encode(&KSA)),
    ] {
        mock::call(ALICE, w, method, &args).unwrap();
    }
    assert_eq!(mock::storage_value(&w, b"config"), config);
    // The views leave BOB's void approval out of what they show, but do not write it away
    assert_eq!(stored_approvals(w, pay), stored);
    assert_eq!(stored.len(), 2);
    assert_eq!(proposal(w, pay).unwrap().approvals, vec![ALICE]);
    assert!(mock::events().is_empty());
}

// ---------------------------------------------------------------------------
// Arithmetic at its limits

#[test]
fn amounts_and_counters_at_their_limits() {
    let w = setup();
    let big = [0x0b; 32];
    mock::register_asset(big, "BIG", 0, [0x99; 32]);
    mock::mint(big, EVE, u128::MAX).unwrap();
    deposit(
        w,
        EVE,
        &[Funds {
            asset: big,
            amount: u128::MAX,
        }],
    )
    .unwrap();
    assert_eq!(balance(w, big), u128::MAX);
    let id = propose(w, ALICE, &transfer(big, DAVE, u128::MAX - 1)).unwrap();
    approve(w, BOB, id).unwrap();
    execute(w, ALICE, id).unwrap();
    assert_eq!(balance(w, big), 1);
    let id = propose(w, ALICE, &transfer(big, DAVE, 1)).unwrap();
    approve(w, BOB, id).unwrap();
    execute(w, ALICE, id).unwrap();
    assert_eq!(
        (balance(w, big), mock::balance(&big, &DAVE)),
        (0, u128::MAX)
    );

    // The next proposal ID would pass u64::MAX
    let owners = [(ALICE, 0), (BOB, 0), (CAROL, 0)];
    write_config(w, &owners, 2, u64::MAX - 1, 0);
    let last = propose(w, ALICE, &Action::AddOwner(DAVE)).unwrap();
    assert_eq!(last, u64::MAX);
    assert_eq!(
        reverted(w, propose(w, ALICE, &transfer(KSA, EVE, 1))),
        "arithmetic overflow"
    );
    // The next term would pass u64::MAX
    approve(w, BOB, last).unwrap();
    write_config(w, &owners, 2, u64::MAX, u64::MAX);
    assert_eq!(reverted(w, execute(w, ALICE, last)), "arithmetic overflow");
    write_config(w, &owners, 2, u64::MAX, u64::MAX - 1);
    execute(w, ALICE, last).unwrap();
    assert_eq!(wallet(w).owners, vec![ALICE, BOB, CAROL, DAVE]);
    assert_eq!(proposal(w, u64::MAX).unwrap().status, Status::Executed);
    assert_eq!(page(w, u64::MAX, 50).len(), 1);
}

// ---------------------------------------------------------------------------
// A random walk against a model of the rules

/// The rules with a removed owner's approvals dropped at once from every pending proposal, where the contract drops
/// them lazily: every outcome must be the same. The wallet's balances are its own accounting of what came in and went
/// out.
struct Model {
    wallet: [u8; 32],
    owners: Vec<[u8; 32]>,
    threshold: u8,
    proposals: Vec<Proposal>,
    balances: BTreeMap<[u8; 32], u128>,
}

type Outcome = Result<(), String>;

fn fail(reason: &str) -> Outcome {
    Err(reason.to_string())
}

impl Model {
    fn owner(&self, who: [u8; 32]) -> Outcome {
        if self.owners.contains(&who) {
            Ok(())
        } else {
            fail("caller is not an owner")
        }
    }

    fn pending(&self, id: u64) -> Result<usize, String> {
        let index = id
            .checked_sub(1)
            .map(|i| i as usize)
            .filter(|i| *i < self.proposals.len())
            .ok_or("unknown proposal")?;
        if self.proposals[index].status != Status::Pending {
            return Err("proposal is not pending".to_string());
        }
        Ok(index)
    }

    fn check(&self, action: &Action) -> Outcome {
        match action {
            Action::Transfer(_, to, amount) => {
                if *to == [0; 32] {
                    return fail("recipient must not be the zero account");
                }
                if *amount == 0 {
                    return fail("amount must be positive");
                }
            }
            Action::Call(contract, method, args, funds) => {
                if *contract == self.wallet {
                    return fail("use the owner actions to change the wallet");
                }
                if *contract == [0; 32] {
                    return fail("contract must not be the zero account");
                }
                if method.is_empty() || method.len() > 64 || method == "init" {
                    return fail("method must be 1 to 64 bytes and not init");
                }
                if args.len() > 16_384 {
                    return fail("args must be at most 16 KiB");
                }
                if funds.len() > 16 {
                    return fail("at most 16 funds");
                }
                if funds.iter().any(|f| f.amount == 0) {
                    return fail("funds amounts must be positive");
                }
                if funds.windows(2).any(|w| w[0].asset >= w[1].asset) {
                    return fail("funds must be sorted by asset without duplicates");
                }
            }
            Action::AddOwner(account) => {
                if *account == [0; 32] {
                    return fail("an owner must not be the zero account");
                }
                if *account == self.wallet {
                    return fail("the wallet cannot own itself");
                }
                if self.owners.contains(account) {
                    return fail("already an owner");
                }
                if self.owners.len() >= 20 {
                    return fail("at most 20 owners");
                }
            }
            Action::RemoveOwner(account) => {
                if !self.owners.contains(account) {
                    return fail("account is not an owner");
                }
                if self.owners.len() - 1 < usize::from(self.threshold) {
                    return fail("removal would leave fewer owners than the threshold");
                }
            }
            Action::SetThreshold(threshold) => {
                if *threshold == 0 || usize::from(*threshold) > self.owners.len() {
                    return fail("threshold must be 1 to the number of owners");
                }
            }
        }
        Ok(())
    }

    fn deposit(&mut self, funds: &[Funds]) {
        for f in funds {
            *self.balances.entry(f.asset).or_default() += f.amount;
        }
    }

    fn propose(&mut self, who: [u8; 32], action: &Action, now: u64) -> Outcome {
        self.owner(who)?;
        self.check(action)?;
        self.proposals.push(Proposal {
            id: self.proposals.len() as u64 + 1,
            proposer: who,
            action: action.clone(),
            approvals: vec![who],
            status: Status::Pending,
            created_at: now,
        });
        Ok(())
    }

    fn approve(&mut self, who: [u8; 32], id: u64) -> Outcome {
        self.owner(who)?;
        let index = self.pending(id)?;
        let approvals = &mut self.proposals[index].approvals;
        if approvals.contains(&who) {
            return fail("already approved");
        }
        approvals.push(who);
        Ok(())
    }

    fn revoke(&mut self, who: [u8; 32], id: u64) -> Outcome {
        self.owner(who)?;
        let index = self.pending(id)?;
        let approvals = &mut self.proposals[index].approvals;
        if !approvals.contains(&who) {
            return fail("not approved by the caller");
        }
        approvals.retain(|a| *a != who);
        Ok(())
    }

    fn cancel(&mut self, who: [u8; 32], id: u64) -> Outcome {
        self.owner(who)?;
        let index = self.pending(id)?;
        if self.proposals[index].proposer != who {
            return fail("only the proposer can cancel");
        }
        self.proposals[index].status = Status::Cancelled;
        Ok(())
    }

    fn execute(&mut self, who: [u8; 32], id: u64) -> Outcome {
        self.owner(who)?;
        let index = self.pending(id)?;
        if self.proposals[index].approvals.len() < usize::from(self.threshold) {
            return fail("not enough approvals");
        }
        let action = self.proposals[index].action.clone();
        self.check(&action)?;
        let held = |asset: &[u8; 32]| self.balances.get(asset).copied().unwrap_or(0);
        let spent: Vec<Funds> = match &action {
            Action::Transfer(asset, _, amount) => vec![Funds {
                asset: *asset,
                amount: *amount,
            }],
            Action::Call(_, _, _, funds) => funds.clone(),
            _ => vec![],
        };
        if spent.iter().any(|f| held(&f.asset) < f.amount) {
            return fail("insufficient balance");
        }
        if let Action::Call(_, method, _, _) = &action {
            if method == "fail" {
                return fail("callee: callee refuses");
            }
        }
        self.proposals[index].status = Status::Executed;
        for f in spent {
            *self.balances.get_mut(&f.asset).unwrap() -= f.amount;
        }
        match action {
            Action::AddOwner(account) => self.owners.push(account),
            Action::RemoveOwner(account) => {
                self.owners.retain(|o| *o != account);
                for p in &mut self.proposals {
                    if p.status == Status::Pending {
                        p.approvals.retain(|a| *a != account);
                    }
                }
            }
            Action::SetThreshold(threshold) => self.threshold = threshold,
            Action::Transfer(..) | Action::Call(..) => {}
        }
        Ok(())
    }
}

/// A small deterministic generator (an LCG, as in the AMM tests).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// The contract's outcome in the model's terms.
fn outcome<T: Debug>(w: [u8; 32], result: Result<T, Failure>) -> Outcome {
    match result {
        Ok(_) => Ok(()),
        Err(Failure::Revert { contract, reason }) if contract == w => {
            Err(String::from_utf8(reason).unwrap())
        }
        Err(Failure::Revert { contract, reason }) if contract == CALLEE => {
            Err(format!("callee: {}", String::from_utf8(reason).unwrap()))
        }
        Err(other) => panic!("unexpected failure {other:?}"),
    }
}

/// Up to two of KSA and USD, each 1 to `most`.
fn random_funds(rng: &mut Rng, most: u64) -> Vec<Funds> {
    let mut amount = || u128::from(rng.below(most) + 1);
    match amount() % 4 {
        0 => vec![],
        1 => vec![ksa(amount())],
        2 => vec![usd(amount())],
        _ => vec![ksa(amount()), usd(amount())],
    }
}

/// Mostly well-formed actions, now and then one of each kind of malformed one.
fn random_action(rng: &mut Rng, people: &[[u8; 32]], w: [u8; 32]) -> Action {
    match rng.below(20) {
        0..=6 => transfer(
            rng.pick(&[KSA, USD]),
            if rng.below(40) == 0 {
                [0; 32]
            } else {
                rng.pick(people)
            },
            if rng.below(40) == 0 {
                0
            } else {
                u128::from(rng.below(1_500) + 1)
            },
        ),
        7..=10 => {
            let method = rng.pick(&["echo", "echo", "echo", "fail", "init", ""]);
            let contract = match rng.below(40) {
                0 => w,
                1 => [0; 32],
                _ => CALLEE,
            };
            let mut args = vec![rng.below(256) as u8];
            let mut funds = random_funds(rng, 1_000);
            match rng.below(40) {
                0 => funds.push(ksa(1)),
                1 => funds.insert(0, usd(1)),
                2 => {
                    funds = (1..=17u8)
                        .map(|i| Funds {
                            asset: [i; 32],
                            amount: 1,
                        })
                        .collect()
                }
                3 => funds.push(Funds {
                    asset: [0xff; 32],
                    amount: 0,
                }),
                4 => args = vec![0; MAX_ARGS_BYTES + 1],
                _ => {}
            }
            Action::Call(contract, method.to_string(), args, funds)
        }
        11..=13 => Action::AddOwner(match rng.below(20) {
            0 => [0; 32],
            1 => w,
            _ => rng.pick(people),
        }),
        14..=16 => Action::RemoveOwner(rng.pick(people)),
        _ => Action::SetThreshold(rng.below(6) as u8),
    }
}

/// Compares everything the views show with the model, and checks that the assets are conserved.
fn compare_all(w: [u8; 32], model: &Model, people: &[[u8; 32]]) {
    assert_eq!(
        wallet(w),
        Wallet {
            owners: model.owners.clone(),
            threshold: model.threshold,
            proposals: model.proposals.len() as u64,
        }
    );
    let mut shown = Vec::new();
    loop {
        let next = page(w, shown.len() as u64 + 1, MAX_PAGE_ITEMS);
        if next.is_empty() {
            break;
        }
        shown.extend(next);
    }
    assert_eq!(shown, model.proposals);
    for asset in [KSA, USD] {
        let held = model.balances.get(&asset).copied().unwrap_or(0);
        assert_eq!(balance(w, asset), held);
        let everyone: u128 = people
            .iter()
            .chain([w, CALLEE].iter())
            .map(|account| mock::balance(&asset, account))
            .sum();
        assert_eq!(everyone, mock::asset_info(&asset).unwrap().total_supply);
    }
}

/// The names and encoded sizes of the events the ABI block declares (fixed-size fields only).
fn declared_events() -> BTreeMap<String, usize> {
    abi_lines()
        .iter()
        .filter_map(|line| line.strip_prefix("event "))
        .map(|event| {
            let (name, fields) = event.split_once('(').unwrap();
            let fields = fields.trim_end_matches(')');
            let size: usize = fields
                .split(',')
                .filter(|field| !field.trim().is_empty())
                .map(|field| match field.split(':').nth(1).unwrap().trim() {
                    "Account" | "AssetId" | "ContractId" | "Hash" => 32,
                    "u128" => 16,
                    "u64" => 8,
                    "u8" => 1,
                    other => panic!("no size for {other}"),
                })
                .sum();
            (name.to_string(), 4 + name.len() + size)
        })
        .collect()
}

#[test]
fn random_walk_matches_the_model() {
    let declared = declared_events();
    let mut emitted = BTreeSet::new();
    let mut outcomes = BTreeSet::new();
    for seed in [1u64, 2, 3] {
        let mut rng = Rng(seed);
        let w = setup();
        let people = [ALICE, BOB, CAROL, DAVE, EVE, FRANK];
        let mut model = Model {
            wallet: w,
            owners: vec![ALICE, BOB, CAROL],
            threshold: 2,
            proposals: Vec::new(),
            balances: BTreeMap::new(),
        };
        let mut now = T0;
        for step in 0..2_500 {
            // The producer's clock: mostly forward, sometimes still or a little behind
            now = match rng.below(10) {
                0 => now - rng.below(30),
                1 => now,
                _ => now + rng.below(600),
            };
            mock::set_block_timestamp(now);
            // Mostly an owner, mostly on one of the latest pending proposals
            let who = if rng.below(4) == 0 {
                rng.pick(&people)
            } else {
                rng.pick(&model.owners)
            };
            let pending: Vec<u64> = model
                .proposals
                .iter()
                .filter(|p| p.status == Status::Pending)
                .map(|p| p.id)
                .collect();
            let latest = &pending[pending.len().saturating_sub(4)..];
            let id = match rng.below(4) {
                _ if pending.is_empty() => rng.below(model.proposals.len() as u64 + 2),
                0 => rng.below(model.proposals.len() as u64 + 2),
                1 => rng.pick(&pending),
                _ => rng.pick(latest),
            };
            let (expected, got) = match rng.below(100) {
                0..=14 => {
                    let funds = random_funds(&mut rng, 1_000);
                    let got = outcome(w, deposit(w, who, &funds));
                    let expected = if funds.is_empty() {
                        fail("attach the assets to deposit")
                    } else {
                        model.deposit(&funds);
                        Ok(())
                    };
                    (expected, got)
                }
                15..=27 => {
                    let action = random_action(&mut rng, &people, w);
                    let got = outcome(w, propose(w, who, &action));
                    (model.propose(who, &action, now), got)
                }
                28..=59 => (model.approve(who, id), outcome(w, approve(w, who, id))),
                60..=65 => (model.revoke(who, id), outcome(w, revoke(w, who, id))),
                66..=95 => (model.execute(who, id), outcome(w, execute(w, who, id))),
                _ => (model.cancel(who, id), outcome(w, cancel(w, who, id))),
            };
            assert_eq!(got, expected, "seed {seed} step {step}");
            outcomes.insert(got.err().unwrap_or_default());
            if let Some(index) = id.checked_sub(1).map(|i| i as usize) {
                if index < model.proposals.len() {
                    assert_eq!(proposal(w, id).as_ref(), model.proposals.get(index));
                }
            }
            for data in events_of(w) {
                let name: String = decode(&data[..4 + data[0] as usize]).unwrap();
                assert_eq!(declared.get(&name), Some(&data.len()), "event {name}");
                emitted.insert(name);
            }
            mock::clear_events();
            if step % 100 == 99 {
                compare_all(w, &model, &people);
            }
        }
        compare_all(w, &model, &people);
        let executed = model
            .proposals
            .iter()
            .filter(|p| p.status == Status::Executed)
            .count();
        assert!(executed >= 50, "seed {seed}: only {executed} executed");
    }
    // The walk reached success and every revert but those it cannot (20 owners, counters at u64::MAX)
    let expected: BTreeSet<String> = [
        "",
        "account is not an owner",
        "already an owner",
        "already approved",
        "amount must be positive",
        "an owner must not be the zero account",
        "args must be at most 16 KiB",
        "at most 16 funds",
        "attach the assets to deposit",
        "callee: callee refuses",
        "caller is not an owner",
        "contract must not be the zero account",
        "funds amounts must be positive",
        "funds must be sorted by asset without duplicates",
        "insufficient balance",
        "method must be 1 to 64 bytes and not init",
        "not approved by the caller",
        "not enough approvals",
        "only the proposer can cancel",
        "proposal is not pending",
        "recipient must not be the zero account",
        "removal would leave fewer owners than the threshold",
        "the wallet cannot own itself",
        "threshold must be 1 to the number of owners",
        "unknown proposal",
        "use the owner actions to change the wallet",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(outcomes, expected);
    // Every declared event was seen, with the size its declared fields give
    assert_eq!(emitted, declared.keys().cloned().collect());
}

// ---------------------------------------------------------------------------
// The ABI block

const SOURCE: &str = include_str!("../src/lib.rs");

/// The declarations of the ABI block, without their descriptions.
fn abi_lines() -> Vec<String> {
    let mut lines = Vec::new();
    let mut inside = false;
    for line in SOURCE.lines() {
        let Some(text) = line.trim().strip_prefix("//!") else {
            continue;
        };
        let text = text.trim();
        if !inside {
            inside = text == "```abi";
            continue;
        }
        if text == "```" {
            return lines;
        }
        let declaration = text.split("//").next().unwrap().trim();
        if !declaration.is_empty() {
            lines.push(declaration.to_string());
        }
    }
    panic!("no closed abi block in the module documentation");
}

/// The method names the ABI block declares (`init`, `call` and `view` lines).
fn declared_methods() -> BTreeSet<String> {
    abi_lines()
        .iter()
        .filter_map(|line| {
            let line = line.strip_prefix("payable ").unwrap_or(line);
            if line.starts_with("init(") {
                return Some("init".to_string());
            }
            let rest = line
                .strip_prefix("call ")
                .or_else(|| line.strip_prefix("view "))?;
            Some(rest.split('(').next().unwrap().to_string())
        })
        .collect()
}

/// The method names of the `"name" =>` arms of `dispatch`.
fn dispatched_methods() -> BTreeSet<String> {
    let body = SOURCE.split("pub fn dispatch(").nth(1).unwrap();
    let body = &body[..body.find("\n}\n").unwrap()];
    body.lines()
        .filter_map(|line| {
            let (name, _) = line.trim().strip_prefix('"')?.split_once("\" =>")?;
            Some(name.to_string())
        })
        .collect()
}

#[test]
fn the_abi_block_matches_dispatch() {
    let declared = declared_methods();
    assert_eq!(declared, dispatched_methods());
    assert_eq!(declared.len(), 11);
    let w = setup();
    for name in &declared {
        let answer = mock::with_context(ALICE, w, || multisig::dispatch(name, &[]));
        assert_ne!(answer, Err(b"unknown method".to_vec()), "{name}");
    }
    assert_eq!(
        mock::with_context(ALICE, w, || multisig::dispatch("transfer", &[])),
        Err(b"unknown method".to_vec())
    );
    // The declared names of the types are those of the contract
    let types: Vec<String> = abi_lines()
        .iter()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("struct ")
                .or_else(|| line.strip_prefix("enum "))?;
            Some(rest.split_whitespace().next().unwrap().to_string())
        })
        .collect();
    assert_eq!(types, ["Funds", "Action", "Status", "Proposal", "Wallet"]);
}
