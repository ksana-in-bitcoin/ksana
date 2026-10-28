//! Tests of the exchange contract on the SDK's in-memory mock host (D-085, D-098): deposit and withdraw, locking (the terms,
//! payment scripts unique forever), claiming (settled at the price, at most the KSA still locked, overpaying buys no more,
//! a partial payment leaves the rest locked for the beneficiary, a payment counts once), expiry and the release delay,
//! and the read-only queries. For the path C claim operation,
//! `mock::set_native_tx` simulates the BTC transaction carrying it and `mock::set_btc_anchor_height` simulates the BTC
//! height anchored by the current block.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use ksa_exchange::{Lock, LockState, MAX_LOCKS_PER_QUERY, RELEASE_DELAY};
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode, Funds, NativeOutput, NativeTx, KSA_ASSET_ID};

const SELLER: [u8; 32] = [0x5e; 32];
const OTHER: [u8; 32] = [0x07; 32];
/// Beneficiary account of the locks (the buyer).
const BUYER: [u8; 32] = [0xb1; 32];
/// One-time signing account of the claim operations (nonce 0).
const EPHEMERAL: [u8; 32] = [0xee; 32];
const SUPPLY: u128 = 1_000_000_000_000;
/// BTC height anchored by the current block when a test starts.
const ANCHOR: u32 = 100;
/// Default expiry height of the locks.
const EXPIRY: u32 = 110;
/// Payment script (P2TR form: OP_1 PUSH32 <x>).
const SCRIPT: [u8; 34] = {
    let mut s = [0x51u8; 34];
    s[1] = 0x20;
    s
};
/// Payment script of a second lock (another P2TR output).
const SCRIPT2: [u8; 34] = {
    let mut s = [0x52u8; 34];
    s[0] = 0x51;
    s[1] = 0x20;
    s
};
const DATA_SCRIPT: [u8; 3] = [0x6a, 0x01, 0x00];
const CHANGE_SCRIPT: [u8; 22] = {
    let mut s = [0x77u8; 22];
    s[0] = 0x00;
    s[1] = 0x14;
    s
};

fn ksa(amount: u128) -> Vec<Funds> {
    vec![Funds {
        asset: KSA_ASSET_ID,
        amount,
    }]
}

fn setup() -> [u8; 32] {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, SELLER, SUPPLY).unwrap();
    mock::mint(KSA_ASSET_ID, OTHER, SUPPLY).unwrap();
    mock::set_btc_anchor_height(ANCHOR);
    mock::deploy(SELLER, 0, b"ksa-exchange", ksa_exchange::dispatch, &[]).unwrap()
}

/// `seller` locks `amount` KSA (with `attached` attached to the call).
#[allow(clippy::too_many_arguments)]
fn lock_with(
    seller: [u8; 32],
    exchange: [u8; 32],
    amount: u128,
    price: u128,
    script: &[u8],
    beneficiary: [u8; 32],
    expiry: u32,
    attached: u128,
) -> Result<u64, Failure> {
    let funds = if attached == 0 { vec![] } else { ksa(attached) };
    mock::call_with_funds(
        seller,
        exchange,
        "lock",
        &encode(&(amount, price, script.to_vec(), beneficiary, expiry)),
        &funds,
    )
    .map(|out| decode(&out).unwrap())
}

/// The seller locks `amount` KSA for the buyer with expiry height [`EXPIRY`].
fn lock(
    exchange: [u8; 32],
    amount: u128,
    price: u128,
    script: &[u8],
    attached: u128,
) -> Result<u64, Failure> {
    lock_with(
        SELLER, exchange, amount, price, script, BUYER, EXPIRY, attached,
    )
}

/// A BTC transaction: a data output, the `payments` to payment scripts, and change.
fn btc_tx(txid: u8, payments: &[(&[u8], u64)]) -> NativeTx {
    let mut outputs = vec![NativeOutput {
        value: 0,
        script: DATA_SCRIPT.to_vec(),
    }];
    outputs.extend(payments.iter().map(|(script, value)| NativeOutput {
        value: *value,
        script: script.to_vec(),
    }));
    outputs.push(NativeOutput {
        value: 12_345,
        script: CHANGE_SCRIPT.to_vec(),
    });
    NativeTx {
        txid: [txid; 32],
        outputs,
    }
}

/// Claims lock `id` in a path C operation carried by `tx` (`None` simulates a path B operation).
fn claim(exchange: [u8; 32], tx: Option<NativeTx>, id: u64) -> Result<(u64, u128), Failure> {
    mock::set_native_tx(tx);
    let result = mock::call(EPHEMERAL, exchange, "claim", &encode(&id));
    mock::set_native_tx(None);
    result.map(|out| decode(&out).unwrap())
}

fn release(exchange: [u8; 32], caller: [u8; 32], id: u64) -> Result<u128, Failure> {
    mock::call(caller, exchange, "release", &encode(&id)).map(|out| decode(&out).unwrap())
}

fn get_lock(exchange: [u8; 32], id: u64) -> Lock {
    decode(&mock::call(OTHER, exchange, "get_lock", &encode(&id)).unwrap()).unwrap()
}

/// The seller's free balance (read-only method `balance`).
fn balance(exchange: [u8; 32], seller: [u8; 32]) -> u128 {
    decode(&mock::call(OTHER, exchange, "balance", &encode(&seller)).unwrap()).unwrap()
}

fn locks_of(exchange: [u8; 32], seller: [u8; 32], start: u64, limit: u32) -> Vec<u64> {
    decode(
        &mock::call(
            OTHER,
            exchange,
            "locks_of",
            &encode(&(seller, start, limit)),
        )
        .unwrap(),
    )
    .unwrap()
}

fn reason(result: Result<impl std::fmt::Debug, Failure>) -> String {
    match result {
        Err(Failure::Revert { reason, .. }) => String::from_utf8(reason).unwrap(),
        other => panic!("expected a revert, got {other:?}"),
    }
}

#[test]
fn a_partial_claim_pays_the_beneficiary_and_leaves_the_rest_locked() {
    let exchange = setup();
    // 1 000 000 KSA attached to the lock call: it first enters the free balance, then all of it moves into the lock
    let id = lock(exchange, 1_000_000, 100, &SCRIPT, 1_000_000).unwrap();
    assert_eq!(id, 0);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &exchange), 1_000_000);
    assert_eq!(balance(exchange, SELLER), 0);
    assert_eq!(
        get_lock(exchange, id),
        Lock {
            seller: SELLER,
            beneficiary: BUYER,
            script: SCRIPT.to_vec(),
            price: 100,
            ksa: 1_000_000,
            expiry: EXPIRY,
            state: LockState::Open,
        }
    );
    // two outputs pay the payment script: 5 000 sats in total → 500 000 KSA credited to the beneficiary account; the other
    // 500 000 stay locked for the beneficiary, nothing returns to the seller
    let tx = btc_tx(1, &[(&SCRIPT, 3_000), (&SCRIPT, 2_000)]);
    assert_eq!(
        claim(exchange, Some(tx.clone()), id).unwrap(),
        (5_000, 500_000)
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 500_000);
    assert_eq!(
        mock::balance(&KSA_ASSET_ID, &EPHEMERAL),
        0,
        "the signing account receives no KSA"
    );
    assert_eq!(balance(exchange, SELLER), 0);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &exchange), 500_000);
    let open = get_lock(exchange, id);
    assert_eq!((open.ksa, open.state), (500_000, LockState::Open));
    let event = mock::events().last().unwrap().data.clone();
    assert_eq!(
        event,
        encode(&(
            "claim",
            id,
            BUYER,
            [1u8; 32],
            5_000u64,
            500_000u128,
            500_000u128
        ))
    );
    // the same payment counts once
    assert_eq!(
        reason(claim(exchange, Some(tx), id)),
        "this transaction has already claimed this lock"
    );
    assert_eq!(
        reason(mock::call(SELLER, exchange, "withdraw", &encode(&1u128))),
        "insufficient free balance"
    );
    // a second payment buys the rest (paying more buys no more): the lock is claimed with the totals of both claims
    assert_eq!(
        claim(exchange, Some(btc_tx(2, &[(&SCRIPT, 6_000)])), id).unwrap(),
        (6_000, 500_000)
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 1_000_000);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &exchange), 0);
    assert_eq!(balance(exchange, SELLER), 0);
    let claimed = get_lock(exchange, id);
    assert_eq!(
        (claimed.ksa, claimed.state),
        (
            0,
            LockState::Claimed {
                paid_sats: 11_000,
                ksa_paid: 1_000_000,
            }
        )
    );
    let event = mock::events().last().unwrap().data.clone();
    assert_eq!(
        event,
        encode(&("claim", id, BUYER, [2u8; 32], 6_000u64, 500_000u128, 0u128))
    );
}

#[test]
fn claims_need_a_native_op_that_pays_an_open_lock() {
    let exchange = setup();
    let id = lock(exchange, 1_000, 1, &SCRIPT, 1_000).unwrap();
    // a path B operation (no BTC transaction carries it)
    assert_eq!(
        reason(claim(exchange, None, id)),
        "claim must be a native (path C) operation"
    );
    // the transaction pays nothing to this lock's payment script
    let other_script = [0x51u8; 20];
    assert_eq!(
        reason(claim(
            exchange,
            Some(btc_tx(2, &[(&other_script, 1_000)])),
            id
        )),
        "the transaction pays nothing to this lock"
    );
    // a lock that does not exist
    assert_eq!(
        reason(claim(exchange, Some(btc_tx(3, &[(&SCRIPT, 10)])), 9)),
        "unknown lock"
    );
    // a partial claim leaves the lock open, but the same transaction cannot claim again (e.g. called twice within one
    // operation via another contract); another payment can
    let tx = btc_tx(4, &[(&SCRIPT, 10)]);
    assert_eq!(claim(exchange, Some(tx.clone()), id).unwrap(), (10, 10));
    assert_eq!(
        reason(claim(exchange, Some(tx), id)),
        "this transaction has already claimed this lock"
    );
    assert_eq!(
        claim(exchange, Some(btc_tx(5, &[(&SCRIPT, 990)])), id).unwrap(),
        (990, 990)
    );
    // once all of it has been bought the lock is no longer Open
    assert_eq!(
        reason(claim(exchange, Some(btc_tx(6, &[(&SCRIPT, 10)])), id)),
        "lock is not open"
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 1_000);
    assert_eq!(balance(exchange, SELLER), 0);
}

#[test]
fn overpayment_buys_at_most_the_locked_ksa() {
    let exchange = setup();
    let id = lock(exchange, 1_000, 3, &SCRIPT, 1_000).unwrap();
    // 334 sats × 3 = 1 002 > 1 000: only the locked 1 000 is received, nothing is returned to the seller
    assert_eq!(
        claim(exchange, Some(btc_tx(1, &[(&SCRIPT, 334)])), id).unwrap(),
        (334, 1_000)
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 1_000);
    assert_eq!(balance(exchange, SELLER), 0);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &exchange), 0);
    // a huge price: saturating multiplication, the claim does not fail on overflow
    let id = lock(exchange, 10, u128::MAX / 2, &SCRIPT2, 10).unwrap();
    assert_eq!(
        claim(exchange, Some(btc_tx(2, &[(&SCRIPT2, 3)])), id).unwrap(),
        (3, 10)
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 1_010);
}

#[test]
fn locked_ksa_is_reserved_until_claimed_or_released() {
    let exchange = setup();
    let out = mock::call_with_funds(SELLER, exchange, "deposit", &[], &ksa(1_000)).unwrap();
    assert_eq!(decode::<u128>(&out).unwrap(), 1_000);
    let id = lock(exchange, 600, 1, &SCRIPT, 0).unwrap();
    let late = lock(exchange, 100, 1, &SCRIPT2, 0).unwrap();
    assert_eq!(balance(exchange, SELLER), 300);
    // the free balance is only 300: locked KSA cannot be withdrawn
    assert_eq!(
        reason(mock::call(SELLER, exchange, "withdraw", &encode(&301u128))),
        "insufficient free balance"
    );
    let out = mock::call(SELLER, exchange, "withdraw", &encode(&300u128)).unwrap();
    assert_eq!(decode::<u128>(&out).unwrap(), 0);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &exchange), 700);
    // no release before the expiry, nor after it until RELEASE_DELAY has passed
    for anchor in [ANCHOR, EXPIRY, EXPIRY + RELEASE_DELAY - 1] {
        mock::set_btc_anchor_height(anchor);
        assert_eq!(
            reason(release(exchange, SELLER, id)),
            "release delay has not passed",
            "anchor {anchor}"
        );
    }
    // a payment after the expiry but before the seller releases is still valid
    mock::set_btc_anchor_height(EXPIRY + 2);
    assert_eq!(
        claim(exchange, Some(btc_tx(1, &[(&SCRIPT2, 100)])), late).unwrap(),
        (100, 100)
    );
    // release once RELEASE_DELAY has passed: the KSA returns to the free balance, the lock becomes Released
    mock::set_btc_anchor_height(EXPIRY + RELEASE_DELAY);
    assert_eq!(release(exchange, SELLER, id).unwrap(), 600);
    assert_eq!(balance(exchange, SELLER), 600);
    assert_eq!(get_lock(exchange, id).state, LockState::Released);
    let event = mock::events().last().unwrap().data.clone();
    assert_eq!(event, encode(&("release", id, 600u128)));
    assert_eq!(reason(release(exchange, SELLER, id)), "lock is not open");
    assert_eq!(
        reason(claim(exchange, Some(btc_tx(2, &[(&SCRIPT, 10)])), id)),
        "lock is not open"
    );
    assert_eq!(reason(release(exchange, SELLER, late)), "lock is not open");
    let out = mock::call(SELLER, exchange, "withdraw", &encode(&600u128)).unwrap();
    assert_eq!(decode::<u128>(&out).unwrap(), 0);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &exchange), 0);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &SELLER), SUPPLY - 100);
}

#[test]
fn only_the_seller_releases() {
    let exchange = setup();
    let id = lock(exchange, 100, 1, &SCRIPT, 100).unwrap();
    mock::set_btc_anchor_height(EXPIRY + RELEASE_DELAY);
    assert_eq!(reason(release(exchange, OTHER, id)), "only the seller");
    assert_eq!(reason(release(exchange, BUYER, id)), "only the seller");
    assert_eq!(reason(release(exchange, SELLER, 7)), "unknown lock");
    assert_eq!(release(exchange, SELLER, id).unwrap(), 100);
}

#[test]
fn lock_rules() {
    let exchange = setup();
    mock::call_with_funds(SELLER, exchange, "deposit", &[], &ksa(1_000)).unwrap();
    assert_eq!(
        reason(lock(exchange, 0, 1, &SCRIPT, 0)),
        "ksa must be positive"
    );
    assert_eq!(
        reason(lock(exchange, 1, 0, &SCRIPT, 0)),
        "price must be positive"
    );
    for bad in [&[][..], &[0x51; 65][..], &DATA_SCRIPT[..]] {
        assert_eq!(
            reason(lock(exchange, 1, 1, bad, 0)),
            "script must be 1 to 64 bytes and not OP_RETURN"
        );
    }
    assert_eq!(
        reason(lock_with(SELLER, exchange, 1, 1, &SCRIPT, BUYER, ANCHOR, 0)),
        "expiry must be above the anchored BTC height"
    );
    assert_eq!(
        reason(lock_with(
            SELLER,
            exchange,
            1,
            1,
            &SCRIPT,
            BUYER,
            u32::MAX - RELEASE_DELAY + 1,
            0
        )),
        "expiry too large"
    );
    assert_eq!(
        reason(lock(exchange, 1_001, 1, &SCRIPT, 0)),
        "insufficient free balance"
    );
    // the attached KSA is counted into the free balance first: 1 000 + 500 suffices to lock 1 200
    let id = lock(exchange, 1_200, 1, &SCRIPT, 500).unwrap();
    assert_eq!(balance(exchange, SELLER), 300);
    // only KSA may be attached
    mock::register_asset([9; 32], "X", 0, [0; 32]);
    mock::mint([9; 32], SELLER, 10).unwrap();
    let result = mock::call_with_funds(
        SELLER,
        exchange,
        "lock",
        &encode(&(1u128, 1u128, SCRIPT2.to_vec(), BUYER, EXPIRY)),
        &[Funds {
            asset: [9; 32],
            amount: 10,
        }],
    );
    assert_eq!(reason(result), "only KSA may be attached");
    // a payment script belongs to a single lock forever: it cannot be reused after a claim or a release either
    assert_eq!(
        reason(lock(exchange, 1, 1, &SCRIPT, 0)),
        "script already used by a lock"
    );
    claim(exchange, Some(btc_tx(1, &[(&SCRIPT, 5)])), id).unwrap();
    assert_eq!(
        reason(lock(exchange, 1, 1, &SCRIPT, 0)),
        "script already used by a lock"
    );
    let released = lock(exchange, 1, 1, &SCRIPT2, 0).unwrap();
    mock::set_btc_anchor_height(EXPIRY + RELEASE_DELAY);
    release(exchange, SELLER, released).unwrap();
    assert_eq!(
        reason(lock_with(
            SELLER,
            exchange,
            1,
            1,
            &SCRIPT2,
            BUYER,
            EXPIRY + 100,
            0
        )),
        "script already used by a lock"
    );
    let found: Option<u64> =
        decode(&mock::call(OTHER, exchange, "lock_of_script", &encode(&SCRIPT.to_vec())).unwrap())
            .unwrap();
    assert_eq!(found, Some(id));
    let found: Option<u64> = decode(
        &mock::call(
            OTHER,
            exchange,
            "lock_of_script",
            &encode(&vec![0x51u8; 20]),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(found, None);
    assert_eq!(
        reason(mock::call(OTHER, exchange, "get_lock", &encode(&99u64))),
        "unknown lock"
    );
    // another seller locks from their own free balance
    assert_eq!(
        lock_with(
            OTHER,
            exchange,
            7,
            2,
            &[0x51u8; 20],
            SELLER,
            EXPIRY + 200,
            7
        )
        .unwrap(),
        released + 1
    );
    assert_eq!(get_lock(exchange, released + 1).seller, OTHER);
}

#[test]
fn init_takes_no_funds() {
    setup();
    let result = mock::deploy_with_funds(
        SELLER,
        1,
        b"ksa-exchange",
        ksa_exchange::dispatch,
        &[],
        &ksa(1),
    );
    assert_eq!(reason(result), "init takes no funds");
}

#[test]
fn deposit_and_withdraw_rules() {
    let exchange = setup();
    assert_eq!(
        reason(mock::call(SELLER, exchange, "deposit", &[])),
        "attach KSA"
    );
    mock::register_asset([9; 32], "X", 0, [0; 32]);
    mock::mint([9; 32], SELLER, 10).unwrap();
    assert_eq!(
        reason(mock::call_with_funds(
            SELLER,
            exchange,
            "deposit",
            &[],
            &[Funds {
                asset: [9; 32],
                amount: 10,
            }]
        )),
        "only KSA may be attached"
    );
    let out = mock::call_with_funds(SELLER, exchange, "deposit", &[], &ksa(300)).unwrap();
    assert_eq!(decode::<u128>(&out).unwrap(), 300);
    let out = mock::call_with_funds(SELLER, exchange, "deposit", &[], &ksa(200)).unwrap();
    assert_eq!(decode::<u128>(&out).unwrap(), 500);
    assert_eq!(balance(exchange, SELLER), 500);
    assert_eq!(balance(exchange, OTHER), 0);
    assert_eq!(
        reason(mock::call(SELLER, exchange, "withdraw", &encode(&0u128))),
        "amount must be positive"
    );
    assert_eq!(
        reason(mock::call(SELLER, exchange, "withdraw", &encode(&501u128))),
        "insufficient free balance"
    );
    assert_eq!(
        reason(mock::call(OTHER, exchange, "withdraw", &encode(&1u128))),
        "insufficient free balance"
    );
    let before = mock::balance(&KSA_ASSET_ID, &SELLER);
    let out = mock::call(SELLER, exchange, "withdraw", &encode(&200u128)).unwrap();
    assert_eq!(decode::<u128>(&out).unwrap(), 300);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &SELLER), before + 200);
    let event = mock::events().last().unwrap().data.clone();
    assert_eq!(event, encode(&("withdraw", SELLER, 200u128, 300u128)));
}

#[test]
fn locks_of_lists_a_sellers_locks_in_order() {
    let exchange = setup();
    let script = |tag: u8| {
        let mut s = vec![0x00, 0x14];
        s.extend([tag; 20]);
        s
    };
    let mut ids = Vec::new();
    ids.push(lock(exchange, 1, 1, &script(0), 1).unwrap());
    ids.push(lock(exchange, 1, 1, &script(1), 1).unwrap());
    let other = lock_with(OTHER, exchange, 1, 1, &script(2), SELLER, EXPIRY, 1).unwrap();
    for tag in 3..6 {
        ids.push(lock(exchange, 1, 1, &script(tag), 1).unwrap());
    }
    assert_eq!(ids, vec![0, 1, 3, 4, 5]);
    assert_eq!(other, 2);
    let count: u64 =
        decode(&mock::call(OTHER, exchange, "lock_count", &encode(&SELLER)).unwrap()).unwrap();
    assert_eq!(count, 5);
    assert_eq!(locks_of(exchange, SELLER, 0, MAX_LOCKS_PER_QUERY), ids);
    assert_eq!(locks_of(exchange, SELLER, 3, MAX_LOCKS_PER_QUERY), ids[3..]);
    assert_eq!(locks_of(exchange, SELLER, 1, 2), ids[1..3]);
    assert!(locks_of(exchange, SELLER, 5, 1).is_empty());
    assert_eq!(locks_of(exchange, OTHER, 0, 1), vec![other]);
    assert!(locks_of(exchange, BUYER, 0, 1).is_empty());
    for limit in [0, MAX_LOCKS_PER_QUERY + 1] {
        assert_eq!(
            reason(mock::call(
                OTHER,
                exchange,
                "locks_of",
                &encode(&(SELLER, 0u64, limit))
            )),
            "limit must be 1 to 256"
        );
    }
}
