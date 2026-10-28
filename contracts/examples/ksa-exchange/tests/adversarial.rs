//! Adversarial tests of the exchange contract (D-085, D-098): after random sequences of deposit / withdraw / lock / claim /
//! release (including all kinds of calls that must not succeed), the KSA held by the contract always equals the free
//! balances of all sellers plus the KSA still locked by open locks, the wallet balances of the accounts match the model,
//! and failed calls change no state; a claim credits KSA only to the beneficiary account stated at lock time, no matter who
//! signs the claim operation, and a payment counts once. A few separate edge cases: an amount of 0 paid to the payment
//! script, the payment in another transaction, the beneficiary account being the contract itself (rejected by `lock`).
//!
//! The M26 cases (SPEC §15, the audit of 2026-10-06): a seller who front-runs the buyer's payment with a few sats paid to
//! its own payment script cannot take the rest of the lock back; what a lock that is never paid costs the seller and the
//! buyer; a lock squatting the seller's payment script.
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

use ksa_exchange::{Lock, LockState, RELEASE_DELAY};
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode, Funds, NativeOutput, NativeTx, KSA_ASSET_ID};

const SELLER: [u8; 32] = [0x5e; 32];
const OTHER: [u8; 32] = [0x07; 32];
const BUYER: [u8; 32] = [0xb1; 32];
const EPHEMERAL: [u8; 32] = [0xee; 32];
const SUPPLY: u128 = 1_000_000_000;
const ANCHOR: u32 = 100;
const DATA_SCRIPT: [u8; 3] = [0x6a, 0x01, 0x00];

/// Accounts with a wallet balance (the sellers, the beneficiary account, the signing account of the claim operations).
const ACCOUNTS: [[u8; 32]; 4] = [SELLER, OTHER, BUYER, EPHEMERAL];

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

/// A payment script in P2TR form: `OP_1 PUSH32 <tag × 32>`.
fn p2tr(tag: u8) -> Vec<u8> {
    let mut script = vec![0x51, 0x20];
    script.extend([tag; 32]);
    script
}

fn reason(result: Result<impl std::fmt::Debug, Failure>) -> String {
    match result {
        Err(Failure::Revert { reason, .. }) => String::from_utf8(reason).unwrap(),
        other => panic!("expected a revert, got {other:?}"),
    }
}

fn free_balance(exchange: [u8; 32], seller: [u8; 32]) -> u128 {
    decode(&mock::call(OTHER, exchange, "balance", &encode(&seller)).unwrap()).unwrap()
}

fn get_lock(exchange: [u8; 32], id: u64) -> Lock {
    decode(&mock::call(OTHER, exchange, "get_lock", &encode(&id)).unwrap()).unwrap()
}

/// A BTC transaction: a data output, some payment outputs, and change.
fn btc_tx(txid: u8, payments: &[(Vec<u8>, u64)]) -> NativeTx {
    let mut outputs = vec![NativeOutput {
        value: 0,
        script: DATA_SCRIPT.to_vec(),
    }];
    outputs.extend(payments.iter().map(|(script, value)| NativeOutput {
        value: *value,
        script: script.clone(),
    }));
    outputs.push(NativeOutput {
        value: 777,
        script: vec![0x00, 0x14, 0x33],
    });
    NativeTx {
        txid: [txid; 32],
        outputs,
    }
}

fn claim(
    exchange: [u8; 32],
    caller: [u8; 32],
    tx: Option<NativeTx>,
    id: u64,
) -> Result<(u64, u128), Failure> {
    mock::set_native_tx(tx);
    let result = mock::call(caller, exchange, "claim", &encode(&id));
    mock::set_native_tx(None);
    result.map(|out| decode(&out).unwrap())
}

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

#[derive(Clone, Debug)]
struct LockModel {
    seller: [u8; 32],
    beneficiary: [u8; 32],
    ksa: u128,
    price: u128,
    script: Vec<u8>,
    expiry: u32,
    open: bool,
}

/// The state the contract should have: the sellers' free balances, the locks, the payment scripts used, the wallet balances
/// of the accounts, the current anchor height, the BTC transactions used so far and the (lock, transaction) pairs that
/// have claimed.
struct Model {
    free: BTreeMap<[u8; 32], u128>,
    locks: Vec<LockModel>,
    scripts: BTreeSet<Vec<u8>>,
    wallets: BTreeMap<[u8; 32], u128>,
    anchor: u32,
    txids: Vec<u8>,
    claimed: BTreeSet<(u64, u8)>,
}

impl Model {
    fn new() -> Model {
        Model {
            free: BTreeMap::new(),
            locks: Vec::new(),
            scripts: BTreeSet::new(),
            wallets: BTreeMap::from([
                (SELLER, SUPPLY),
                (OTHER, SUPPLY),
                (BUYER, 0),
                (EPHEMERAL, 0),
            ]),
            anchor: ANCHOR,
            txids: Vec::new(),
            claimed: BTreeSet::new(),
        }
    }

    fn free(&self, account: [u8; 32]) -> u128 {
        self.free.get(&account).copied().unwrap_or(0)
    }

    fn held_by_contract(&self) -> u128 {
        self.free.values().sum::<u128>()
            + self
                .locks
                .iter()
                .filter(|l| l.open)
                .map(|l| l.ksa)
                .sum::<u128>()
    }

    /// The contract matches the model item by item.
    fn check(&self, exchange: [u8; 32], step: usize) {
        assert_eq!(
            mock::balance(&KSA_ASSET_ID, &exchange),
            self.held_by_contract(),
            "step {step}: contract balance = free balances + open locks"
        );
        for account in ACCOUNTS {
            assert_eq!(
                free_balance(exchange, account),
                self.free(account),
                "step {step}: free balance of {:02x}",
                account[0]
            );
            assert_eq!(
                mock::balance(&KSA_ASSET_ID, &account),
                self.wallets[&account],
                "step {step}: wallet of {:02x}",
                account[0]
            );
        }
        for (id, lock) in self.locks.iter().enumerate() {
            let actual = get_lock(exchange, id as u64);
            assert_eq!(
                (
                    actual.seller,
                    actual.beneficiary,
                    actual.ksa,
                    actual.price,
                    actual.expiry
                ),
                (
                    lock.seller,
                    lock.beneficiary,
                    lock.ksa,
                    lock.price,
                    lock.expiry
                ),
                "step {step}: lock {id}"
            );
            assert_eq!(actual.script, lock.script, "step {step}: lock {id}");
            assert_eq!(
                matches!(actual.state, LockState::Open),
                lock.open,
                "step {step}: lock {id}"
            );
        }
    }
}

#[test]
fn random_sequences_keep_the_contract_balance_equal_to_free_balances_plus_open_locks() {
    for seed in 1..=4u64 {
        run_random_sequence(seed, 400);
    }
}

fn run_random_sequence(seed: u64, steps: usize) {
    let exchange = setup();
    let mut model = Model::new();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x1234_5678_9abc_def1));
    let mut next_script_tag = 1u8;
    let mut next_txid = 1u8;
    let mut successes = BTreeMap::<&str, u32>::new();
    for step in 0..steps {
        match rng.below(10) {
            // deposit: amount 0 fails
            0 => {
                let seller = rng.pick(&[SELLER, OTHER]);
                let amount = rng.below(2_001) as u128;
                let funds = if amount == 0 { vec![] } else { ksa(amount) };
                let result = mock::call_with_funds(seller, exchange, "deposit", &[], &funds);
                let expected = amount > 0;
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "step {step} deposit {amount}: {result:?}"
                );
                if expected {
                    *model.free.entry(seller).or_default() += amount;
                    *model.wallets.get_mut(&seller).unwrap() -= amount;
                    *successes.entry("deposit").or_default() += 1;
                }
            }
            // withdraw: any account, any amount; succeeds only when the account's own free balance suffices
            1 | 2 => {
                let account = rng.pick(&ACCOUNTS);
                let amount = rng.below(2_001) as u128;
                let result = mock::call(account, exchange, "withdraw", &encode(&amount));
                let expected = amount > 0 && model.free(account) >= amount;
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "step {step} withdraw {amount}: {result:?}"
                );
                if expected {
                    *model.free.get_mut(&account).unwrap() -= amount;
                    *model.wallets.get_mut(&account).unwrap() += amount;
                    *successes.entry("withdraw").or_default() += 1;
                }
            }
            // lock: all kinds of terms, including invalid ones
            3 | 4 => {
                let seller = rng.pick(&[SELLER, OTHER]);
                let amount = rng.below(1_501) as u128;
                let price = rng.below(51) as u128;
                let script = match rng.below(10) {
                    0 if !model.locks.is_empty() => model.locks
                        [rng.below(model.locks.len() as u64) as usize]
                        .script
                        .clone(),
                    1 => vec![],
                    2 => vec![0x51; 65],
                    3 => DATA_SCRIPT.to_vec(),
                    _ => {
                        next_script_tag = next_script_tag.wrapping_add(1);
                        p2tr(next_script_tag)
                    }
                };
                let beneficiary = if rng.below(12) == 0 {
                    exchange
                } else {
                    rng.pick(&ACCOUNTS)
                };
                let expiry = match rng.below(10) {
                    0 => model.anchor,
                    1 => u32::MAX - RELEASE_DELAY + 1,
                    _ => model.anchor + 1 + rng.below(12) as u32,
                };
                let attached = rng.below(1_001) as u128;
                let funds = if attached == 0 { vec![] } else { ksa(attached) };
                let result = mock::call_with_funds(
                    seller,
                    exchange,
                    "lock",
                    &encode(&(amount, price, script.clone(), beneficiary, expiry)),
                    &funds,
                );
                let script_ok = !script.is_empty()
                    && script.len() <= 64
                    && script.first() != Some(&0x6a)
                    && !model.scripts.contains(&script);
                let expected = amount > 0
                    && price > 0
                    && script_ok
                    && beneficiary != exchange
                    && expiry > model.anchor
                    && expiry <= u32::MAX - RELEASE_DELAY
                    && model.free(seller) + attached >= amount;
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "step {step} lock {amount} @ {price} expiry {expiry}: {result:?}"
                );
                if expected {
                    let id: u64 = decode(&result.unwrap()).unwrap();
                    assert_eq!(id as usize, model.locks.len());
                    *model.free.entry(seller).or_default() += attached;
                    *model.free.get_mut(&seller).unwrap() -= amount;
                    *model.wallets.get_mut(&seller).unwrap() -= attached;
                    model.scripts.insert(script.clone());
                    model.locks.push(LockModel {
                        seller,
                        beneficiary,
                        ksa: amount,
                        price,
                        script,
                        expiry,
                        open: true,
                    });
                    *successes.entry("lock").or_default() += 1;
                }
            }
            // claim (path C): signed by anyone; the payment may be 0 and may also pay another lock's script; now and then the
            // transaction of an earlier claim comes again
            5 | 6 => {
                let id = rng.below(model.locks.len() as u64 + 2);
                let caller = rng.pick(&ACCOUNTS);
                let sats = rng.below(41);
                let mut payments = Vec::new();
                let script = model
                    .locks
                    .get(id as usize)
                    .map(|l| l.script.clone())
                    .unwrap_or_else(|| p2tr(0));
                // the amount is split into one or two outputs
                if sats > 0 && rng.below(2) == 0 {
                    let first = rng.below(sats);
                    payments.push((script.clone(), first));
                    payments.push((script.clone(), sats - first));
                } else {
                    payments.push((script.clone(), sats));
                }
                if !model.locks.is_empty() && rng.below(3) == 0 {
                    let other = &model.locks[rng.below(model.locks.len() as u64) as usize];
                    payments.push((other.script.clone(), 1 + rng.below(20)));
                }
                let txid = if !model.txids.is_empty() && rng.below(8) == 0 {
                    model.txids[rng.below(model.txids.len() as u64) as usize]
                } else {
                    next_txid = next_txid.checked_add(1).unwrap();
                    model.txids.push(next_txid);
                    next_txid
                };
                let result = claim(exchange, caller, Some(btc_tx(txid, &payments)), id);
                // the sum of all outputs paying this lock's payment script (the "other lock" may happen to be this one)
                let sats: u64 = payments
                    .iter()
                    .filter(|(s, _)| *s == script)
                    .map(|(_, v)| *v)
                    .sum();
                let expected = model.locks.get(id as usize).is_some_and(|l| l.open)
                    && sats > 0
                    && !model.claimed.contains(&(id, txid));
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "step {step} claim {id} with {sats} sats in tx {txid}: {result:?}"
                );
                if expected {
                    let lock = &mut model.locks[id as usize];
                    let paid = u128::from(sats).saturating_mul(lock.price).min(lock.ksa);
                    assert_eq!(result.unwrap(), (sats, paid));
                    // the KSA bought goes to the beneficiary; the rest stays locked until it is bought or released
                    *model.wallets.get_mut(&lock.beneficiary).unwrap() += paid;
                    lock.ksa -= paid;
                    lock.open = lock.ksa > 0;
                    model.claimed.insert((id, txid));
                    *successes.entry("claim").or_default() += 1;
                }
            }
            // claim (path B): always fails
            7 => {
                let id = rng.below(model.locks.len() as u64 + 1);
                let result = claim(exchange, rng.pick(&ACCOUNTS), None, id);
                assert!(result.is_err(), "step {step}: path B claim {id} succeeded");
            }
            // release: anyone, any time
            8 => {
                // half of the time pick a lock that has already expired and release it as the seller; otherwise random
                // (including nonexistent numbers, other people's locks, not yet expired)
                let releasable: Vec<u64> = model
                    .locks
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.open && model.anchor >= l.expiry + RELEASE_DELAY)
                    .map(|(i, _)| i as u64)
                    .collect();
                let (id, caller) = if !releasable.is_empty() && rng.below(2) == 0 {
                    let id = releasable[rng.below(releasable.len() as u64) as usize];
                    let caller = if rng.below(4) == 0 {
                        rng.pick(&ACCOUNTS)
                    } else {
                        model.locks[id as usize].seller
                    };
                    (id, caller)
                } else {
                    (rng.below(model.locks.len() as u64 + 1), rng.pick(&ACCOUNTS))
                };
                let result = mock::call(caller, exchange, "release", &encode(&id));
                let expected = model.locks.get(id as usize).is_some_and(|l| {
                    l.open && l.seller == caller && model.anchor >= l.expiry + RELEASE_DELAY
                });
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "step {step} release {id} by {:02x}: {result:?}",
                    caller[0]
                );
                if expected {
                    let lock = &mut model.locks[id as usize];
                    assert_eq!(decode::<u128>(&result.unwrap()).unwrap(), lock.ksa);
                    *model.free.entry(lock.seller).or_default() += lock.ksa;
                    lock.open = false;
                    *successes.entry("release").or_default() += 1;
                }
            }
            // BTC advances
            _ => {
                model.anchor += rng.below(5) as u32;
                mock::set_btc_anchor_height(model.anchor);
            }
        }
        model.check(exchange, step);
    }
    // every kind of call succeeded at least once: the scenario is not running idle
    for kind in ["deposit", "withdraw", "lock", "claim", "release"] {
        assert!(
            successes.get(kind).copied().unwrap_or(0) > 0,
            "seed {seed}: no successful {kind}"
        );
    }
}

#[test]
fn only_outputs_of_the_carrying_transaction_that_pay_the_lock_count() {
    let exchange = setup();
    let id: u64 = decode(
        &mock::call_with_funds(
            SELLER,
            exchange,
            "lock",
            &encode(&(1_000u128, 10u128, p2tr(1), BUYER, ANCHOR + 10)),
            &ksa(1_000),
        )
        .unwrap(),
    )
    .unwrap();
    // an output of amount 0 does not count as a payment
    assert_eq!(
        reason(claim(
            exchange,
            EPHEMERAL,
            Some(btc_tx(1, &[(p2tr(1), 0)])),
            id
        )),
        "the transaction pays nothing to this lock"
    );
    // the payment is in another transaction: the transaction carrying the claim operation pays nothing, so the claim fails
    // (the contract only looks at the transaction carrying it)
    let _paid_elsewhere = btc_tx(2, &[(p2tr(1), 500)]);
    assert_eq!(
        reason(claim(
            exchange,
            EPHEMERAL,
            Some(btc_tx(3, &[(p2tr(9), 500)])),
            id
        )),
        "the transaction pays nothing to this lock"
    );
    // a payment script that differs by a single byte does not count
    let mut near = p2tr(1);
    near[2] ^= 1;
    assert_eq!(
        reason(claim(
            exchange,
            EPHEMERAL,
            Some(btc_tx(4, &[(near, 500)])),
            id
        )),
        "the transaction pays nothing to this lock"
    );
    assert!(matches!(get_lock(exchange, id).state, LockState::Open));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 0);
    // the position of the payment outputs is irrelevant: placed before the data output or split into several, the total
    // counts
    let tx = NativeTx {
        txid: [5; 32],
        outputs: vec![
            NativeOutput {
                value: 30,
                script: p2tr(1),
            },
            NativeOutput {
                value: 0,
                script: DATA_SCRIPT.to_vec(),
            },
            NativeOutput {
                value: 20,
                script: p2tr(1),
            },
        ],
    };
    assert_eq!(claim(exchange, OTHER, Some(tx), id).unwrap(), (50, 500));
    assert_eq!(
        mock::balance(&KSA_ASSET_ID, &BUYER),
        500,
        "the KSA is credited to the beneficiary account regardless of the signer"
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &OTHER), SUPPLY);
    // the other 500 stay locked for the beneficiary
    assert_eq!(free_balance(exchange, SELLER), 0);
    assert_eq!(get_lock(exchange, id).ksa, 500);
    assert!(matches!(get_lock(exchange, id).state, LockState::Open));
}

/// The beneficiary account set to the contract itself: on claim the KSA would be "transferred" to the contract itself and
/// no account could ever take that part out again (the seller's own loss); other sellers are unaffected, and the KSA held by
/// the contract is still no less than it should be.
#[test]
fn a_lock_whose_beneficiary_is_the_contract_itself_is_rejected() {
    let exchange = setup();
    // nobody can take out KSA credited to the contract itself, so lock rejects such a beneficiary account; the attached KSA
    // is returned with the revert and no lock is left behind
    let result = mock::call_with_funds(
        SELLER,
        exchange,
        "lock",
        &encode(&(1_000u128, 1u128, p2tr(1), exchange, ANCHOR + 10)),
        &ksa(1_000),
    );
    assert!(reason(result).contains("beneficiary"));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &SELLER), SUPPLY);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &exchange), 0);
    assert_eq!(free_balance(exchange, SELLER), 0);
    assert!(
        reason(mock::call(OTHER, exchange, "get_lock", &encode(&0u64))).contains("unknown lock")
    );
    // the same terms with the buyer account instead lock fine; the payment script was not taken by the failed call
    let id: u64 = decode(
        &mock::call_with_funds(
            SELLER,
            exchange,
            "lock",
            &encode(&(1_000u128, 1u128, p2tr(1), BUYER, ANCHOR + 10)),
            &ksa(1_000),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(id, 0);
    assert_eq!(get_lock(exchange, 0).beneficiary, BUYER);
}

/// F-06 of the 2026-10-06 audit (fixed by D-098). The seller locks 1 000 000 KSA at 100 per sat for the buyer, with a
/// payment script of its own. Before the buyer's payment of 10 000 sats is force-included (the claim of a path C payment
/// only executes `NATIVE_CONFIRMATIONS` BTC blocks after it confirms, and until then `get_lock` still says `Open`), the
/// seller confirms a transaction of its own that pays 330 sats to the same script and carries a claim signed by a key of
/// its own. When a partial claim closed the lock and returned the rest to the seller, this left the buyer with 33 000 KSA
/// for 10 000 sats and the seller with the BTC and the other 967 000 KSA. Now the seller's claim only buys 33 000 KSA for
/// the buyer, the rest stays locked, and the buyer's payment buys it.
#[test]
fn a_seller_front_running_with_dust_cannot_take_the_lock_back() {
    let exchange = setup();
    let seller_script = p2tr(0x5e);
    let id: u64 = decode(
        &mock::call_with_funds(
            SELLER,
            exchange,
            "lock",
            &encode(&(
                1_000_000u128,
                100u128,
                seller_script.clone(),
                BUYER,
                ANCHOR + 20,
            )),
            &ksa(1_000_000),
        )
        .unwrap(),
    )
    .unwrap();
    // the buyer reads the lock and pays for all of it: ceil(1 000 000 / 100) sats
    let lock = get_lock(exchange, id);
    let sats_for_all = lock.ksa.div_ceil(lock.price) as u64;
    assert_eq!(sats_for_all, 10_000);
    // the seller's dust claim lands first; anyone can sign a claim, the KSA still goes to the beneficiary
    assert_eq!(
        claim(
            exchange,
            SELLER,
            Some(btc_tx(1, &[(seller_script.clone(), 330)])),
            id
        )
        .unwrap(),
        (330, 33_000)
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 33_000);
    assert_eq!(
        free_balance(exchange, SELLER),
        0,
        "nothing returns to the seller"
    );
    assert!(
        reason(mock::call(SELLER, exchange, "withdraw", &encode(&1u128)))
            .contains("insufficient free balance")
    );
    assert_eq!(get_lock(exchange, id).ksa, 967_000);
    // the buyer's payment, made on the old reading of the lock, still gets all of the KSA (it overpays by the 330 sats
    // the seller paid itself)
    assert_eq!(
        claim(
            exchange,
            EPHEMERAL,
            Some(btc_tx(2, &[(seller_script, sats_for_all)])),
            id
        )
        .unwrap(),
        (10_000, 967_000)
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 1_000_000);
    assert_eq!(
        get_lock(exchange, id).state,
        LockState::Claimed {
            paid_sats: 10_330,
            ksa_paid: 1_000_000,
        }
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &exchange), 0);
}

/// A third party pays some sats to a lock's payment script (griefing, or to make the buyer's claim fail): it only gives its
/// share of the KSA to the beneficiary at its own expense; a claim that pays more than the KSA still locked buys the rest
/// and the overpaid part is lost by whoever paid it.
#[test]
fn third_party_claims_only_give_the_beneficiary_ksa() {
    let exchange = setup();
    let script = p2tr(0x31);
    let id: u64 = decode(
        &mock::call_with_funds(
            SELLER,
            exchange,
            "lock",
            &encode(&(1_000u128, 1u128, script.clone(), BUYER, ANCHOR + 20)),
            &ksa(1_000),
        )
        .unwrap(),
    )
    .unwrap();
    for (txid, sats) in [(1u8, 100u64), (2, 250)] {
        claim(
            exchange,
            OTHER,
            Some(btc_tx(txid, &[(script.clone(), sats)])),
            id,
        )
        .unwrap();
    }
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 350);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &OTHER), SUPPLY);
    // the buyer pays for all 1 000 as quoted before those claims: it gets the 650 left
    assert_eq!(
        claim(exchange, EPHEMERAL, Some(btc_tx(3, &[(script, 1_000)])), id).unwrap(),
        (1_000, 650)
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 1_000);
    assert_eq!(free_balance(exchange, SELLER), 0);
}

/// Lock without pay (SPEC §15 "the cost of lock-without-pay in the exchange"): the buyer asks for a lock and never pays.
/// The buyer spends nothing on chain; the seller pays the sponsored fees of `lock` and `release` and cannot use the KSA
/// from the lock until the anchor reaches `expiry + RELEASE_DELAY`. The shortest possible lock (expiry = the anchor + 1)
/// therefore keeps the KSA for `RELEASE_DELAY + 1` BTC blocks after the lock executes (about 70 minutes on mainnet);
/// a seller who gives a buyer more time loses that much more, and nothing in the contract makes the buyer pay for it (an
/// off-chain deposit, a reputation, or a short expiry are the seller's tools). A payment made before the seller releases
/// still counts, so the seller releases only once the delay has passed.
#[test]
fn a_lock_that_is_never_paid_costs_the_seller_the_lock_period() {
    let exchange = setup();
    let id: u64 = decode(
        &mock::call_with_funds(
            SELLER,
            exchange,
            "lock",
            &encode(&(5_000u128, 10u128, p2tr(0x77), BUYER, ANCHOR + 1)),
            &ksa(5_000),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 0);
    for anchor in ANCHOR..ANCHOR + 1 + RELEASE_DELAY {
        mock::set_btc_anchor_height(anchor);
        assert!(
            reason(mock::call(SELLER, exchange, "release", &encode(&id)))
                .contains("release delay has not passed"),
            "anchor {anchor}"
        );
        assert!(
            reason(mock::call(SELLER, exchange, "withdraw", &encode(&1u128)))
                .contains("insufficient free balance")
        );
    }
    // RELEASE_DELAY + 1 BTC blocks after the lock: the seller takes it back and withdraws
    mock::set_btc_anchor_height(ANCHOR + 1 + RELEASE_DELAY);
    let out = mock::call(SELLER, exchange, "release", &encode(&id)).unwrap();
    assert_eq!(decode::<u128>(&out).unwrap(), 5_000);
    mock::call(SELLER, exchange, "withdraw", &encode(&5_000u128)).unwrap();
    assert_eq!(mock::balance(&KSA_ASSET_ID, &SELLER), SUPPLY);
    // a partly paid lock is the same: the part bought stays with the buyer, the rest waits for the release
    let id: u64 = decode(
        &mock::call_with_funds(
            SELLER,
            exchange,
            "lock",
            &encode(&(
                5_000u128,
                10u128,
                p2tr(0x78),
                BUYER,
                ANCHOR + 1 + RELEASE_DELAY + 1,
            )),
            &ksa(5_000),
        )
        .unwrap(),
    )
    .unwrap();
    claim(
        exchange,
        EPHEMERAL,
        Some(btc_tx(9, &[(p2tr(0x78), 100)])),
        id,
    )
    .unwrap();
    assert!(
        reason(mock::call(SELLER, exchange, "release", &encode(&id))).contains("release delay")
    );
    mock::set_btc_anchor_height(ANCHOR + 2 * (1 + RELEASE_DELAY));
    let out = mock::call(SELLER, exchange, "release", &encode(&id)).unwrap();
    assert_eq!(decode::<u128>(&out).unwrap(), 4_000);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BUYER), 1_000);
}

/// Script squatting: someone who sees the seller's `lock` operation in the entry pool locks 1 base unit with the same
/// payment script first, and the seller's lock fails ("script already used by a lock"). It costs the squatter its fee and
/// 1 base unit until its own lock expires, gains it nothing (a payment to the script still pays the seller's address, and
/// the squatter's lock is not the lock the buyer was told about), and the seller only needs a fresh payment address for
/// the retry. Clients resolve locks by id and check the terms, never by script.
#[test]
fn a_squatted_payment_script_only_forces_a_new_address() {
    let exchange = setup();
    let seller_script = p2tr(0x5e);
    // the squatter (OTHER) locks first with the seller's script
    let squat: u64 = decode(
        &mock::call_with_funds(
            OTHER,
            exchange,
            "lock",
            &encode(&(1u128, 1u128, seller_script.clone(), OTHER, ANCHOR + 1)),
            &ksa(1),
        )
        .unwrap(),
    )
    .unwrap();
    let result = mock::call_with_funds(
        SELLER,
        exchange,
        "lock",
        &encode(&(1_000u128, 1u128, seller_script, BUYER, ANCHOR + 20)),
        &ksa(1_000),
    );
    assert!(reason(result).contains("script already used by a lock"));
    assert_eq!(
        mock::balance(&KSA_ASSET_ID, &SELLER),
        SUPPLY,
        "the failed lock keeps nothing"
    );
    // the seller retries with a fresh address
    let id: u64 = decode(
        &mock::call_with_funds(
            SELLER,
            exchange,
            "lock",
            &encode(&(1_000u128, 1u128, p2tr(0x5f), BUYER, ANCHOR + 20)),
            &ksa(1_000),
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(id, squat);
    assert_eq!(get_lock(exchange, id).seller, SELLER);
    assert_eq!(get_lock(exchange, squat).seller, OTHER);
}
