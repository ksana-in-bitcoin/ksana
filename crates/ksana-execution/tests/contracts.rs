//! Verifies the M1 acceptance items with the real example contracts (`contracts/examples`, compiled to wasm by
//! `ksana-test-contracts`):
//!
//! 1. deploy and call the example token contract; balances are correct after transfers;
//! 2. when a contract reverts / traps / exceeds `max_gas`, all state changes of the op are rolled back, the nonce is
//!    incremented and the fee is still charged;
//!
//! and those of M41 with the token standards' reference implementation in the contract SDK (`docs/STANDARDS.md`): the
//! KS-20 token and a KS-721 collection (`contracts/testing/collection`) declare themselves at deployment and refuse
//! what the standards refuse, and `transfer_and_call` passes the attached funds on to the recipient
//! (`contracts/testing/receiver`), returns its answer and fails the whole op when the recipient refuses.
//!
//! Contract state is always read through the contracts' query methods (black box), never relying on their internal
//! storage layout.

mod common;

use std::collections::BTreeMap;

use borsh::BorshSerialize;
use common::*;
use ksana_execution::builtin::contract_id;
use ksana_execution::{OpStatus, Receipt};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Funds};
use ksana_test_contracts::{AMM, AMM_FACTORY, COLLECTION, RECEIVER, TOKEN};

const GAS: u64 = 20_000_000;
const FEE: u128 = 7;

fn args<T: BorshSerialize>(v: &T) -> Vec<u8> {
    borsh::to_vec(v).unwrap()
}

fn call<T: BorshSerialize>(contract: [u8; 32], method: &str, a: &T) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args: args(a),
        funds: Vec::new(),
    }
}

fn call0(contract: [u8; 32], method: &str) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args: vec![],
        funds: Vec::new(),
    }
}

/// Borsh encoding of the AMM example's `Side::Token(id)`: variant 0 + ID.
fn token_side(id: [u8; 32]) -> (u8, [u8; 32]) {
    (0, id)
}

/// The AMM example's `Side::Asset(id)`: variant 1 + ID (built-in asset, D-073).
fn asset_side(id: [u8; 32]) -> (u8, [u8; 32]) {
    (1, id)
}

fn token_init(name: &str, symbol: &str, decimals: u8, supply: u128) -> Vec<u8> {
    args(&(name.to_string(), symbol.to_string(), decimals, supply))
}

/// Borsh encoding of the token event `("transfer", from, to, amount)`.
fn transfer_event(from: [u8; 32], to: [u8; 32], amount: u128) -> Vec<u8> {
    args(&("transfer".to_string(), from, to, amount))
}

/// A small chain executing every op as its own block; tracks each account's nonce, queries are paid by a dedicated
/// account.
struct World {
    chain: Chain,
    nonces: BTreeMap<[u8; 32], u64>,
}

impl World {
    fn new(users: &[&User]) -> Self {
        let mut allocations: Vec<_> = users.iter().map(|u| (u.account, 1_000_000)).collect();
        allocations.push((user("viewer").account, 1_000_000));
        World {
            chain: Chain::new(allocations),
            nonces: BTreeMap::new(),
        }
    }

    fn run_as(&mut self, account: [u8; 32], u: &User, max_gas: u64, action: Action) -> Receipt {
        let nonce = self.nonces.entry(account).or_insert(0);
        let entry = sponsored(u, *nonce, FEE, max_gas, action);
        *nonce += 1;
        let mut out = self
            .chain
            .block(vec![entry])
            .expect("block should be valid");
        assert_eq!(out.receipts.len(), 1);
        out.receipts.remove(0)
    }

    fn run(&mut self, u: &User, max_gas: u64, action: Action) -> Receipt {
        self.run_as(u.account, u, max_gas, action)
    }

    fn ok(&mut self, u: &User, action: Action) -> Receipt {
        let r = self.run(u, GAS, action);
        assert_eq!(r.status, OpStatus::Success, "{r:?}");
        r
    }

    fn deploy(&mut self, u: &User, code: &[u8], init_args: Vec<u8>) -> [u8; 32] {
        self.deploy_receipt(u, code, init_args).0
    }

    fn deploy_receipt(&mut self, u: &User, code: &[u8], init_args: Vec<u8>) -> ([u8; 32], Receipt) {
        let id = contract_id(&u.account, self.nonce(u), &sha256(code));
        let r = self.ok(
            u,
            Action::Deploy {
                code: code.to_vec(),
                init_args,
                funds: Vec::new(),
            },
        );
        (id, r)
    }

    /// Calls a read-only method as the query account and returns the decoded result.
    fn query<T: borsh::BorshDeserialize>(&mut self, action: Action) -> T {
        let viewer = user("viewer");
        let r = self.run_as(viewer.account, &viewer, GAS, action);
        assert_eq!(r.status, OpStatus::Success, "{r:?}");
        borsh::from_slice(&r.output).unwrap()
    }

    fn balance(&mut self, token: [u8; 32], who: &User) -> u128 {
        self.query(call(token, "balance_of", &who.account))
    }

    fn nonce(&self, u: &User) -> u64 {
        self.nonces.get(&u.account).copied().unwrap_or(0)
    }

    fn ksa(&self, u: &User) -> u128 {
        self.chain.ksa(&u.account)
    }
}

fn is_failed(r: &Receipt) -> bool {
    matches!(r.status, OpStatus::Failed(_))
}

#[test]
fn token_deploy_call_and_transfers_update_balances() {
    let (a, b, c) = (user("a"), user("b"), user("c"));
    let mut w = World::new(&[&a, &b, &c]);

    let token = w.deploy(&a, TOKEN, token_init("Test Token", "TST", 6, 1_000_000));
    let meta: (String, String, u8) = w.query(call0(token, "metadata"));
    assert_eq!(meta, ("Test Token".into(), "TST".into(), 6));
    assert_eq!(w.query::<u128>(call0(token, "total_supply")), 1_000_000);
    assert_eq!(w.balance(token, &a), 1_000_000);

    let r = w.ok(&a, call(token, "transfer", &(b.account, 250_000u128)));
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].contract, token);
    assert_eq!(
        r.events[0].data,
        transfer_event(a.account, b.account, 250_000)
    );
    w.ok(&b, call(token, "transfer", &(c.account, 100_000u128)));
    w.ok(&c, call(token, "transfer", &(a.account, 1u128)));

    assert_eq!(w.balance(token, &a), 750_001);
    assert_eq!(w.balance(token, &b), 150_000);
    assert_eq!(w.balance(token, &c), 99_999);
    assert_eq!(w.query::<u128>(call0(token, "total_supply")), 1_000_000);

    // Every op is charged the KSA fee and increments the nonce as usual
    assert_eq!(w.ksa(&a), 1_000_000 - 2 * FEE);
    assert_eq!(w.chain.nonce(&a.account), 2);
    assert_eq!(w.chain.nonce(&c.account), 1);

    // Approved transfer: b approves c, c transfers from b to a
    w.ok(&b, call(token, "approve", &(c.account, 5_000u128)));
    w.ok(
        &c,
        call(token, "transfer_from", &(b.account, a.account, 4_000u128)),
    );
    assert_eq!(
        w.query::<u128>(call(token, "allowance", &(b.account, c.account))),
        1_000
    );
    assert_eq!(w.balance(token, &a), 754_001);
    assert_eq!(w.balance(token, &b), 146_000);
}

#[test]
fn token_revert_rolls_back_state_but_keeps_fee_and_nonce() {
    let (a, b, c) = (user("a"), user("b"), user("c"));
    let mut w = World::new(&[&a, &b, &c]);
    let token = w.deploy(&a, TOKEN, token_init("T", "T", 0, 100));

    // Insufficient balance: revert
    let (ksa, nonce) = (w.ksa(&b), w.chain.nonce(&b.account));
    let r = w.run(&b, GAS, call(token, "transfer", &(c.account, 1u128)));
    assert!(is_failed(&r), "{r:?}");
    assert!(format!("{:?}", r.status).contains("revert"), "{r:?}");
    assert_eq!(r.gas_used, GAS, "a failed op is charged max_gas");
    assert!(r.events.is_empty() && r.output.is_empty());
    assert_eq!(w.ksa(&b), ksa - FEE);
    assert_eq!(w.chain.nonce(&b.account), nonce + 1);
    assert_eq!(w.balance(token, &b), 0);
    assert_eq!(w.balance(token, &c), 0);

    // Insufficient allowance: revert, neither the allowance nor the balances change
    w.ok(&a, call(token, "approve", &(c.account, 10u128)));
    let r = w.run(
        &c,
        GAS,
        call(token, "transfer_from", &(a.account, c.account, 11u128)),
    );
    assert!(is_failed(&r), "{r:?}");
    assert_eq!(
        w.query::<u128>(call(token, "allowance", &(a.account, c.account))),
        10
    );
    assert_eq!(w.balance(token, &a), 100);
}

#[test]
fn out_of_gas_after_all_writes_rolls_back_everything() {
    let (a, b) = (user("a"), user("b"));
    let mut w = World::new(&[&a, &b]);
    let token = w.deploy(&a, TOKEN, token_init("T", "T", 0, 1_000));
    let transfer_one = call(token, "transfer", &(b.account, 1u128));

    // Make b's balance key exist first; afterwards every transfer of 1 has exactly the same execution path and storage
    // read/write lengths, hence the same gas
    w.ok(&a, transfer_one.clone());
    let exact = w.ok(&a, transfer_one.clone()).gas_used;
    assert_eq!(w.balance(token, &a), 998);

    // 1 gas short: it runs out only after all storage writes and events, which must all be rolled back
    let (ksa, nonce) = (w.ksa(&a), w.chain.nonce(&a.account));
    let r = w.run(&a, exact - 1, transfer_one.clone());
    assert!(is_failed(&r), "{r:?}");
    assert!(format!("{:?}", r.status).contains("gas"), "{r:?}");
    assert_eq!(r.gas_used, exact - 1);
    assert!(r.events.is_empty());
    assert_eq!(w.balance(token, &a), 998);
    assert_eq!(w.balance(token, &b), 2);
    assert_eq!(w.ksa(&a), ksa - FEE, "the fee is still charged");
    assert_eq!(
        w.chain.nonce(&a.account),
        nonce + 1,
        "the nonce is incremented"
    );

    // Succeeds with exactly enough, showing the failure above was indeed short by just the last 1 gas
    let r = w.run(&a, exact, transfer_one);
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(r.gas_used, exact);
    assert_eq!(w.balance(token, &a), 997);
    assert_eq!(w.balance(token, &b), 3);
}

#[test]
fn amm_nested_calls_and_nested_failure_rollback() {
    let (a, trader) = (user("a"), user("trader"));
    let mut w = World::new(&[&a, &trader]);
    let ta = w.deploy(&a, TOKEN, token_init("A", "A", 0, 1_000_000));
    let tb = w.deploy(&a, TOKEN, token_init("B", "B", 0, 1_000_000));
    let amm = w.deploy(&a, AMM, args(&(token_side(ta), token_side(tb))));

    // Adding liquidity without approval: the AMM first updates the reserves and shares, then calls the token
    // contract's transfer_from, which reverts. A failing callee fails the whole op (ABI §2) and the receipt carries the
    // callee's failure reason; the reserves and shares the AMM already wrote and the token balances are all rolled
    // back.
    let (ksa, nonce) = (w.ksa(&a), w.chain.nonce(&a.account));
    let r = w.run(
        &a,
        GAS,
        call(amm, "add_liquidity", &(10_000u128, 40_000u128)),
    );
    assert!(is_failed(&r), "{r:?}");
    assert!(
        format!("{:?}", r.status).contains("insufficient allowance"),
        "{r:?}"
    );
    assert_eq!(w.query::<(u128, u128)>(call0(amm, "reserves")), (0, 0));
    assert_eq!(w.query::<u128>(call(amm, "shares_of", &a.account)), 0);
    assert_eq!(w.query::<u128>(call0(amm, "total_shares")), 0);
    assert_eq!(w.balance(ta, &a), 1_000_000);
    assert_eq!(w.balance(tb, &a), 1_000_000);
    assert_eq!(w.ksa(&a), ksa - FEE);
    assert_eq!(w.chain.nonce(&a.account), nonce + 1);

    // Adding after approval: initial shares = isqrt(10 000 · 40 000) = 20 000
    w.ok(&a, call(ta, "approve", &(amm, 10_000u128)));
    w.ok(&a, call(tb, "approve", &(amm, 40_000u128)));
    let r = w.ok(&a, call(amm, "add_liquidity", &(10_000u128, 40_000u128)));
    assert_eq!(borsh::from_slice::<u128>(&r.output).unwrap(), 20_000);
    assert_eq!(
        w.query::<(u128, u128)>(call0(amm, "reserves")),
        (10_000, 40_000)
    );
    assert_eq!(w.balance(ta, &a), 990_000);

    // Swap: out = in·997·R_out / (R_in·1000 + in·997)
    w.ok(&a, call(ta, "transfer", &(trader.account, 1_000u128)));
    w.ok(&trader, call(ta, "approve", &(amm, 1_000u128)));
    let expected_out = 1_000u128 * 997 * 40_000 / (10_000 * 1_000 + 1_000 * 997);
    assert_eq!(expected_out, 3_626);
    let r = w.ok(&trader, call(amm, "swap", &(ta, 1_000u128, 3_626u128)));
    assert_eq!(borsh::from_slice::<u128>(&r.output).unwrap(), expected_out);
    assert_eq!(w.balance(ta, &trader), 0);
    assert_eq!(w.balance(tb, &trader), expected_out);
    assert_eq!(
        w.query::<(u128, u128)>(call0(amm, "reserves")),
        (11_000, 40_000 - expected_out)
    );

    // Slippage protection: reverts when min_out is above the obtainable amount, reserves unchanged
    w.ok(&a, call(ta, "transfer", &(trader.account, 100u128)));
    w.ok(&trader, call(ta, "approve", &(amm, 100u128)));
    let r = w.run(&trader, GAS, call(amm, "swap", &(ta, 100u128, 1_000u128)));
    assert!(is_failed(&r), "{r:?}");
    assert_eq!(
        w.query::<(u128, u128)>(call0(amm, "reserves")),
        (11_000, 40_000 - expected_out)
    );
    assert_eq!(w.balance(ta, &trader), 100);
}

fn ksa(amount: u128) -> Funds {
    Funds {
        asset: [0; 32],
        amount,
    }
}

fn call_funds<T: BorshSerialize>(
    contract: [u8; 32],
    method: &str,
    a: &T,
    funds: Vec<Funds>,
) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args: args(a),
        funds,
    }
}

#[test]
fn amm_trades_ksa_against_a_token_with_attached_funds() {
    let (a, trader) = (user("a"), user("trader"));
    let mut w = World::new(&[&a, &trader]);
    let tb = w.deploy(&a, TOKEN, token_init("B", "B", 0, 1_000_000));
    let amm = w.deploy(&a, AMM, args(&(asset_side([0; 32]), token_side(tb))));

    // The KSA side via attached funds, the token side via approval
    w.ok(&a, call(tb, "approve", &(amm, 40_000u128)));
    let ksa_before = w.ksa(&a);
    let r = w.ok(
        &a,
        call_funds(
            amm,
            "add_liquidity",
            &(10_000u128, 40_000u128),
            vec![ksa(10_000)],
        ),
    );
    assert_eq!(borsh::from_slice::<u128>(&r.output).unwrap(), 20_000);
    assert_eq!(w.ksa(&a), ksa_before - 10_000 - FEE);
    assert_eq!(w.chain.ksa(&amm), 10_000);

    // KSA → token
    let expected = 1_000u128 * 997 * 40_000 / (10_000 * 1_000 + 1_000 * 997);
    let r = w.ok(
        &trader,
        call_funds(
            amm,
            "swap",
            &([0u8; 32], 1_000u128, 1u128),
            vec![ksa(1_000)],
        ),
    );
    assert_eq!(borsh::from_slice::<u128>(&r.output).unwrap(), expected);
    assert_eq!(w.balance(tb, &trader), expected);
    assert_eq!(w.chain.ksa(&amm), 11_000);

    // Token → KSA: the KSA is transferred out of the AMM's balance
    w.ok(&trader, call(tb, "approve", &(amm, expected)));
    let ksa_before = w.ksa(&trader);
    let r = w.ok(&trader, call(amm, "swap", &(tb, expected, 1u128)));
    let back = borsh::from_slice::<u128>(&r.output).unwrap();
    assert!(back > 0 && back < 1_000);
    assert_eq!(w.ksa(&trader), ksa_before + back - FEE);
    assert_eq!(w.chain.ksa(&amm), 11_000 - back);

    // Attached amount does not match: revert, the attached funds are returned with it
    let ksa_before = w.ksa(&trader);
    let r = w.run(
        &trader,
        GAS,
        call_funds(amm, "swap", &([0u8; 32], 500u128, 1u128), vec![ksa(400)]),
    );
    assert!(is_failed(&r), "{r:?}");
    assert_eq!(w.ksa(&trader), ksa_before - FEE);
}

#[test]
fn factory_creates_pairs_with_create_contract() {
    let a = user("a");
    let mut w = World::new(&[&a]);
    let tb = w.deploy(&a, TOKEN, token_init("B", "B", 0, 1_000_000));
    // The AMM code is put on chain by one deployment first; the factory only records the code hash
    let tc = w.deploy(&a, TOKEN, token_init("C", "C", 0, 1_000_000));
    w.deploy(&a, AMM, args(&(token_side(tb), token_side(tc))));
    let amm_code = sha256(AMM);
    let factory = w.deploy(&a, AMM_FACTORY, args(&amm_code));

    let r = w.ok(
        &a,
        call(
            factory,
            "create_pair",
            &(token_side(tb), asset_side([0; 32])),
        ),
    );
    let pair: [u8; 32] = borsh::from_slice(&r.output).unwrap();
    // salt = sha256(Borsh(both sides in canonical order)): KSA (all-zero ID) first
    let salt = sha256(&args(&(asset_side([0; 32]), token_side(tb))));
    assert_eq!(
        pair,
        ksana_execution::builtin::created_contract_id(&factory, &salt, &amm_code)
    );
    let found: Option<[u8; 32]> = w.query(call(
        factory,
        "get_pair",
        &(asset_side([0; 32]), token_side(tb)),
    ));
    assert_eq!(found, Some(pair));

    // Creating the same pair again fails
    let r = w.run(
        &a,
        GAS,
        call(
            factory,
            "create_pair",
            &(asset_side([0; 32]), token_side(tb)),
        ),
    );
    assert!(is_failed(&r), "{r:?}");

    // The pair can be used directly
    w.ok(&a, call(tb, "approve", &(pair, 4_000u128)));
    let r = w.ok(
        &a,
        call_funds(
            pair,
            "add_liquidity",
            &(1_000u128, 4_000u128),
            vec![ksa(1_000)],
        ),
    );
    assert_eq!(borsh::from_slice::<u128>(&r.output).unwrap(), 2_000);
    assert_eq!(w.chain.ksa(&pair), 1_000);
}

/// The events of a receipt, as `(contract, data)`.
fn events_of(r: &Receipt) -> Vec<([u8; 32], Vec<u8>)> {
    r.events
        .iter()
        .map(|e| (e.contract, e.data.clone()))
        .collect()
}

fn failed_with(r: &Receipt, reason: &str) -> bool {
    matches!(&r.status, OpStatus::Failed(why) if why.contains(reason))
}

/// A KS-721 token ID from a serial number, as `ks721::id` makes it: the number big-endian in the last 16 bytes.
fn nft_id(serial: u128) -> [u8; 32] {
    let mut id = [0; 32];
    id[16..].copy_from_slice(&serial.to_be_bytes());
    id
}

#[test]
fn the_ks20_token_declares_itself_and_refuses_what_the_standard_refuses() {
    let (a, b) = (user("a"), user("b"));
    let mut w = World::new(&[&a, &b]);
    let (token, r) = w.deploy_receipt(&a, TOKEN, token_init("Test Token", "TST", 6, 1_000));
    let standards: Vec<String> = ["KS-20", "KS-20-burnable", "KS-20-mintable"]
        .map(String::from)
        .to_vec();
    assert_eq!(
        events_of(&r),
        [
            (token, args(&("standards", &standards))),
            (token, args(&("metadata", "Test Token", "TST", 6u8))),
            (token, args(&("minter", None::<[u8; 32]>, None::<[u8; 32]>))),
            (token, transfer_event([0; 32], a.account, 1_000)),
        ]
    );
    assert_eq!(w.query::<Vec<String>>(call0(token, "standards")), standards);
    assert!(w
        .query::<Vec<String>>(call0(token, "capabilities"))
        .is_empty());

    // The attached KSA comes back with every refusal
    for (action, reason) in [
        (call(token, "transfer", &(b.account, 0u128)), "zero amount"),
        (call(token, "transfer", &([0u8; 32], 1u128)), "zero account"),
        (
            call(token, "transfer", &(token, 1u128)),
            "recipient is this contract",
        ),
        (
            call_funds(token, "transfer", &(b.account, 1u128), vec![ksa(5)]),
            "unexpected attached funds",
        ),
        (
            call(token, "approve", &(a.account, 1u128, None::<u64>)),
            "spender is the caller",
        ),
    ] {
        let before = w.ksa(&a);
        let r = w.run(&a, GAS, action);
        assert!(failed_with(&r, reason), "{reason}: {r:?}");
        assert_eq!(w.ksa(&a), before - FEE);
    }
    assert_eq!(w.balance(token, &a), 1_000);

    // An allowance with an expiry: the approve event carries it
    let expires_at = w.chain.timestamp() + 3_600;
    let r = w.ok(
        &a,
        call(token, "approve", &(b.account, 300u128, Some(expires_at))),
    );
    assert_eq!(
        events_of(&r),
        [(
            token,
            args(&("approve", a.account, b.account, 300u128, Some(expires_at)))
        )]
    );
    w.ok(
        &b,
        call(token, "transfer_from", &(a.account, b.account, 100u128)),
    );
    assert_eq!(
        w.query::<(u128, Option<u64>)>(call(token, "allowance_info", &(a.account, b.account))),
        (200, Some(expires_at))
    );
}

#[test]
fn ks20_transfer_and_call_passes_the_funds_on_and_returns_the_answer() {
    let (a, b) = (user("a"), user("b"));
    let mut w = World::new(&[&a, &b]);
    let token = w.deploy(&a, TOKEN, token_init("T", "T", 0, 1_000));
    let receiver = w.deploy(&a, RECEIVER, Vec::new());

    let before = w.ksa(&a);
    let r = w.ok(
        &a,
        call_funds(
            token,
            "transfer_and_call",
            &(receiver, 100u128, b"hi".to_vec()),
            vec![ksa(50)],
        ),
    );
    // The token returns the recipient's answer, Bytes, unchanged: here the token and the amount it received
    let answer: Vec<u8> = borsh::from_slice(&r.output).unwrap();
    assert_eq!(
        borsh::from_slice::<([u8; 32], u128)>(&answer).unwrap(),
        (token, 100)
    );
    assert_eq!(
        events_of(&r),
        [(token, transfer_event(a.account, receiver, 100))]
    );
    assert_eq!(w.query::<u128>(call(token, "balance_of", &receiver)), 100);
    assert_eq!(w.chain.ksa(&receiver), 50);
    assert_eq!(w.ksa(&a), before - 50 - FEE);
    // The recipient was called by the token, with the sender, the amount, the data and the funds
    let seen: Option<([u8; 32], Vec<u8>, Vec<Funds>)> = w.query(call0(receiver, "last"));
    assert_eq!(
        seen,
        Some((
            token,
            args(&(a.account, 100u128, b"hi".to_vec())),
            vec![ksa(50)]
        ))
    );

    // A refusal fails the whole op: the tokens and the funds stay with the sender
    let before = w.ksa(&a);
    let r = w.run(
        &a,
        GAS,
        call_funds(
            token,
            "transfer_and_call",
            &(receiver, 10u128, b"refuse".to_vec()),
            vec![ksa(5)],
        ),
    );
    assert!(failed_with(&r, "refused"), "{r:?}");
    assert_eq!(w.balance(token, &a), 900);
    assert_eq!(w.chain.ksa(&receiver), 50);
    assert_eq!(w.ksa(&a), before - FEE);

    // A contract without on_token_received, and an account that is no contract, take nothing
    let other = w.deploy(&a, TOKEN, token_init("U", "U", 0, 1));
    for to in [other, b.account] {
        let r = w.run(
            &a,
            GAS,
            call(token, "transfer_and_call", &(to, 1u128, Vec::<u8>::new())),
        );
        assert!(is_failed(&r), "{r:?}");
    }
    assert_eq!(w.balance(token, &a), 900);
    assert_eq!(w.balance(token, &b), 0);
}

#[test]
fn a_ks721_collection_moves_tokens_and_calls_the_recipient() {
    let (a, b) = (user("a"), user("b"));
    let mut w = World::new(&[&a, &b]);
    let (c, r) = w.deploy_receipt(&a, COLLECTION, args(&("Test Collection", "TC")));
    let standards: Vec<String> = ["KS-721", "KS-721-burnable", "KS-721-enumerable"]
        .map(String::from)
        .to_vec();
    assert_eq!(
        events_of(&r),
        [
            (c, args(&("standards", &standards))),
            (c, args(&("metadata", "Test Collection", "TC"))),
        ]
    );
    let receiver = w.deploy(&a, RECEIVER, Vec::new());
    let r = w.ok(&a, call(c, "mint", &(a.account, 1u128, Some("ipfs://one"))));
    assert_eq!(
        events_of(&r),
        [(c, args(&("transfer", [0u8; 32], a.account, nft_id(1))))]
    );
    w.ok(&a, call(c, "mint", &(a.account, 2u128, None::<String>)));
    assert_eq!(
        w.query::<Option<String>>(call(c, "token_uri", &nft_id(1))),
        Some("ipfs://one".to_string())
    );

    let r = w.ok(&a, call(c, "transfer", &(b.account, nft_id(1))));
    assert_eq!(
        events_of(&r),
        [(c, args(&("transfer", a.account, b.account, nft_id(1))))]
    );

    // transfer_and_call: the funds go on to the recipient, which answers with the collection and the token
    let before = w.ksa(&a);
    let r = w.ok(
        &a,
        call_funds(
            c,
            "transfer_and_call",
            &(receiver, nft_id(2), b"hi".to_vec()),
            vec![ksa(7)],
        ),
    );
    let answer: Vec<u8> = borsh::from_slice(&r.output).unwrap();
    assert_eq!(
        borsh::from_slice::<([u8; 32], [u8; 32])>(&answer).unwrap(),
        (c, nft_id(2))
    );
    assert_eq!(
        w.query::<Option<[u8; 32]>>(call(c, "owner_of", &nft_id(2))),
        Some(receiver)
    );
    assert_eq!(w.chain.ksa(&receiver), 7);
    assert_eq!(w.ksa(&a), before - 7 - FEE);

    // A refusal fails the whole op, and only the owner or someone it approved moves a token
    let r = w.run(
        &b,
        GAS,
        call(
            c,
            "transfer_and_call",
            &(receiver, nft_id(1), b"refuse".to_vec()),
        ),
    );
    assert!(failed_with(&r, "refused"), "{r:?}");
    let r = w.run(&a, GAS, call(c, "transfer", &(a.account, nft_id(1))));
    assert!(failed_with(&r, "not the owner"), "{r:?}");
    let r = w.run(
        &a,
        GAS,
        call(c, "transfer_from", &(b.account, a.account, nft_id(1))),
    );
    assert!(failed_with(&r, "not authorized"), "{r:?}");
    assert_eq!(
        w.query::<Option<[u8; 32]>>(call(c, "owner_of", &nft_id(1))),
        Some(b.account)
    );

    // The lists, and a burned ID is never minted again
    assert_eq!(w.query::<u64>(call0(c, "total_supply")), 2);
    assert_eq!(
        w.query::<Vec<[u8; 32]>>(call(c, "tokens_of", &(b.account, 0u64, 10u32))),
        [nft_id(1)]
    );
    w.ok(&b, call(c, "burn", &nft_id(1)));
    assert_eq!(
        w.query::<Vec<[u8; 32]>>(call(c, "tokens", &(0u64, 10u32))),
        [nft_id(2)]
    );
    let r = w.run(
        &a,
        GAS,
        call(c, "mint", &(a.account, 1u128, None::<String>)),
    );
    assert!(failed_with(&r, "token was burned"), "{r:?}");
}
