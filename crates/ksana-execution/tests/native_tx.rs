//! The `native_tx` host function (D-078): a path-C op reads the transaction ID and all outputs of the BTC transaction
//! carrying it, other paths and read-only calls read "none"; the result depends only on the BTC view (the anchor
//! chain), and a view failure is a local fault. The exchange contract `ksa-exchange` uses it for top-ups (D-085): the
//! seller locks KSA (price, a payment script used only for this lock, beneficiary account, expiry height), the buyer's
//! payment transaction carries a claim op signed by a one-time key (nonce 0), the KSA it buys is credited to the
//! beneficiary account fixed at lock time and the rest stays locked for the beneficiary (D-098); the seller can only take
//! it back after expiry plus the release delay.

mod common;

use std::sync::Arc;

use common::probe::*;
use common::*;
use ksana_execution::builtin::contract_id;
use ksana_execution::{BlockError, OpStatus, Receipt};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Entry, DEVNET};
use ksana_test_contracts::KSA_EXCHANGE;
use ksana_vm::{NativeOutput, NativeTxInfo};

/// A path-C op carried by transaction `btc_txid` (the signature is bound to that transaction's first input, D-086).
fn carried(u: &User, nonce: u64, btc_txid: [u8; 32], action: Action) -> Entry {
    Entry::Forced {
        btc_txid,
        op: native_op(u, nonce, btc_txid, DEVNET.native_max_gas, action),
    }
}

/// Executes a block containing only `entry` and returns its receipt.
fn run_block(w: &mut World, entry: Entry) -> Receipt {
    w.chain
        .block(vec![entry])
        .expect("block should be valid")
        .receipts
        .remove(0)
}

fn info(txid: [u8; 32], outputs: &[(u64, Vec<u8>)]) -> NativeTxInfo {
    NativeTxInfo {
        txid,
        outputs: outputs
            .iter()
            .map(|(value, script)| NativeOutput {
                value: *value,
                script: script.clone(),
            })
            .collect(),
    }
}

#[test]
fn native_ops_read_the_transaction_that_carries_them() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);

    // Path B: there is no BTC transaction carrying it
    let r = w.run(&alice, call(p, "native_tx", vec![], vec![]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(
        borsh::from_slice::<Option<NativeTxInfo>>(&r.output).unwrap(),
        None
    );

    // Path C: all outputs of the transaction given by the BTC view (including the KSN1 data output), in transaction
    // order
    let txid = [0x7c; 32];
    let outputs = vec![(0u64, vec![0x6a, 0x01, 0x00]), (5_000, vec![0x51; 34])];
    w.chain.btc.native.insert(txid, outputs.clone());
    let carrier = user("carrier");
    let r = run_block(
        &mut w,
        carried(&carrier, 0, txid, call(p, "native_tx", vec![], vec![])),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(
        borsh::from_slice::<Option<NativeTxInfo>>(&r.output).unwrap(),
        Some(info(txid, &outputs))
    );
    assert_eq!(w.chain.nonce(&carrier.account), 1);

    // Read-only call (ksana_call): none either
    let r = w
        .chain
        .exec
        .simulate_call(
            Arc::new(w.chain.state.clone()),
            Arc::new(w.chain.btc.clone()),
            &w.chain.env(),
            alice.account,
            p,
            "native_tx".into(),
            vec![],
            vec![],
            GAS,
        )
        .unwrap();
    assert_eq!(
        borsh::from_slice::<Option<NativeTxInfo>>(&r.output).unwrap(),
        None
    );

    // The BTC view cannot find the transaction: local fault, the block is not judged invalid
    let unknown = carried(
        &user("unknown"),
        0,
        [0x55; 32],
        call(p, "native_tx", vec![], vec![]),
    );
    match w.chain.block(vec![unknown]) {
        Err(BlockError::Fatal(m)) => assert!(m.contains("BTC data source failure"), "{m}"),
        other => panic!("should be a local fault: {other:?}"),
    }
}

/// A P2TR-style payment script: `OP_1 PUSH32 <tag × 32>`.
fn p2tr(tag: u8) -> Vec<u8> {
    let mut script = vec![0x51, 0x20];
    script.extend([tag; 32]);
    script
}

/// The seller locks `ksa_amount` (attached to the call) for the buyer; returns the lock ID.
#[allow(clippy::too_many_arguments)]
fn lock(
    w: &mut World,
    exchange: [u8; 32],
    seller: &User,
    buyer: &User,
    ksa_amount: u128,
    price: u128,
    script: Vec<u8>,
    expiry: u32,
) -> u64 {
    let r = w.run(
        seller,
        call(
            exchange,
            "lock",
            enc(&(ksa_amount, price, script, buyer.account, expiry)),
            vec![ksa(ksa_amount)],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    borsh::from_slice(&r.output).unwrap()
}

#[test]
fn a_top_up_claim_turns_a_btc_payment_into_ksa() {
    let (seller, buyer) = (user("seller"), user("buyer"));
    let mut w = World::new(&[&seller]);
    let r = w.run(
        &seller,
        Action::Deploy {
            code: KSA_EXCHANGE.to_vec(),
            init_args: vec![],
            funds: vec![],
        },
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let exchange = contract_id(&seller.account, 0, &sha256(KSA_EXCHANGE));

    // The test chain anchors at BTC height 200. Seller lock: 3 KSA base units per sat, 100 000 KSA (attached to the
    // call), beneficiary is the buyer, expiry 300
    let script = p2tr(0x42);
    let id = lock(
        &mut w,
        exchange,
        &seller,
        &buyer,
        100_000,
        3,
        script.clone(),
        300,
    );
    assert_eq!(id, 0);
    assert_eq!(w.chain.ksa(&exchange), 100_000);

    // The buyer's BTC transaction: pays 10 000 sats to the payment script and carries a claim op signed by a one-time
    // key (nonce 0) with just the lock ID
    let txid = [0xab; 32];
    let data = vec![0x6a, 0x01, 0x00];
    let change = vec![0x00, 0x14, 0x11, 0x22];
    w.chain.btc.native.insert(
        txid,
        vec![
            (0, data.clone()),
            (10_000, script.clone()),
            (50_000, change),
        ],
    );
    let r = run_block(
        &mut w,
        carried(
            &user("one-time-1"),
            0,
            txid,
            call(exchange, "claim", enc(&0u64), vec![]),
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(
        borsh::from_slice::<(u64, u128)>(&r.output).unwrap(),
        (10_000, 30_000)
    );
    assert!(r.gas_used < DEVNET.native_max_gas, "{}", r.gas_used);
    assert_eq!(w.chain.ksa(&buyer.account), 30_000);
    assert_eq!(w.chain.ksa(&user("one-time-1").account), 0);
    // The unsold 70 000 stay locked for the buyer: the seller cannot withdraw them
    let r = w.run(&seller, call(exchange, "withdraw", enc(&1u128), vec![]));
    assert!(failed(&r).contains("insufficient free balance"), "{r:?}");
    assert_eq!(w.chain.ksa(&exchange), 70_000);
    // A second payment of the buyer buys the rest (23 334 sats × 3 = 70 002 > 70 000)
    let txid_rest = [0xae; 32];
    w.chain
        .btc
        .native
        .insert(txid_rest, vec![(0, data.clone()), (23_334, script.clone())]);
    let r = run_block(
        &mut w,
        carried(
            &user("one-time-4"),
            0,
            txid_rest,
            call(exchange, "claim", enc(&0u64), vec![]),
        ),
    );
    assert_eq!(
        borsh::from_slice::<(u64, u128)>(&r.output).unwrap(),
        (23_334, 70_000),
        "{r:?}"
    );
    assert_eq!(w.chain.ksa(&buyer.account), 100_000);
    assert_eq!(w.chain.ksa(&exchange), 0);

    // Once credited, the buyer can use path B immediately
    let r = w.run(&buyer, transfer(seller.account, 1_000));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(w.chain.ksa(&buyer.account), 100_000 - 1_000 - FEE);

    // Second lock (new payment script): overpaying buys no more — 20 000 sats × 3 = 60 000 > the locked 50 000
    let script2 = p2tr(0x43);
    let id = lock(
        &mut w,
        exchange,
        &seller,
        &buyer,
        50_000,
        3,
        script2.clone(),
        300,
    );
    assert_eq!(id, 1);
    let txid2 = [0xac; 32];
    w.chain
        .btc
        .native
        .insert(txid2, vec![(20_000, script2.clone()), (0, data.clone())]);
    let r = run_block(
        &mut w,
        carried(
            &user("one-time-2"),
            0,
            txid2,
            call(exchange, "claim", enc(&1u64), vec![]),
        ),
    );
    assert_eq!(
        borsh::from_slice::<(u64, u128)>(&r.output).unwrap(),
        (20_000, 50_000),
        "{r:?}"
    );
    assert_eq!(w.chain.ksa(&buyer.account), 100_000 - 1_000 - FEE + 50_000);
    assert_eq!(w.chain.ksa(&exchange), 0);

    // Paying again for an already claimed lock: the claim fails (a path-C failure only consumes the one-time account's
    // nonce, D-018), the BTC has been paid to the seller
    let txid3 = [0xad; 32];
    w.chain
        .btc
        .native
        .insert(txid3, vec![(0, data.clone()), (5_000, script2)]);
    let r = run_block(
        &mut w,
        carried(
            &user("one-time-3"),
            0,
            txid3,
            call(exchange, "claim", enc(&1u64), vec![]),
        ),
    );
    assert!(failed(&r).contains("lock is not open"), "{r:?}");
    assert_eq!(w.chain.nonce(&user("one-time-3").account), 1);

    // Claim via path B: the contract reverts (only path-C ops carry a payment transaction), the op fails and is charged
    let r = w.run(&buyer, call(exchange, "claim", enc(&0u64), vec![]));
    assert!(failed(&r).contains("native (path C)"), "{r:?}");

    // Nobody pays for the third lock: anchor 200 has not reached "expiry 201 + RELEASE_DELAY 6", the seller cannot
    // release
    let id = lock(&mut w, exchange, &seller, &buyer, 1_000, 1, p2tr(0x44), 201);
    assert_eq!(id, 2);
    let r = w.run(&seller, call(exchange, "release", enc(&2u64), vec![]));
    assert!(failed(&r).contains("release delay has not passed"), "{r:?}");
    assert_eq!(w.chain.ksa(&exchange), 1_000);

    // Release in a block anchored at 207: the KSA returns to the seller's available balance in the contract
    let mut env = w.chain.env();
    env.btc_anchor.height = 207;
    let nonce = w.chain.nonce(&seller.account);
    let release = sponsored(
        &seller,
        nonce,
        FEE,
        GAS,
        call(exchange, "release", enc(&2u64), vec![]),
    );
    let out = w
        .chain
        .exec
        .execute_chain_block(
            Arc::new(w.chain.state.clone()),
            Arc::new(w.chain.btc.clone()),
            &env,
            &[],
            &[release],
        )
        .expect("block should be valid");
    let r = &out.receipts[0];
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(borsh::from_slice::<u128>(&r.output).unwrap(), 1_000);
    w.chain.state.apply(&out.writes);
    let r = w
        .chain
        .exec
        .simulate_call(
            Arc::new(w.chain.state.clone()),
            Arc::new(w.chain.btc.clone()),
            &env,
            buyer.account,
            exchange,
            "balance".into(),
            enc(&seller.account),
            vec![],
            GAS,
        )
        .unwrap();
    assert_eq!(
        borsh::from_slice::<u128>(&r.output).unwrap(),
        1_000,
        "{r:?}"
    );
    assert_eq!(w.chain.ksa(&exchange), 1_000);
}
