//! Example contract: lock-based exchange of BTC for KSA (top-up, D-078, D-085).
//!
//! The flow is "order → lock → confirm → redeem":
//!
//! 1. **Order** (off chain): buyer and seller agree on the quantity, the price (how many KSA base units per sat) and the
//!    buyer's Ksana account.
//! 2. **Lock** (seller, path B): the seller first deposits KSA with `deposit` (or attaches it to `lock`), then `lock` moves
//!    an amount of KSA from the seller's own free balance into a lock, stating the price, a BTC payment script used by this
//!    one lock only, the beneficiary account (the buyer) and the expiry BTC height. The terms of a lock cannot be changed,
//!    and the seller cannot take locked KSA away (`withdraw` only touches the free balance): before paying, the buyer can
//!    confirm that the KSA has already been reserved for them, and the seller cannot pull it out ahead of the payment.
//! 3. **Confirm** (buyer, path C): the buyer sends an ordinary BTC transaction that contains both an output paying the
//!    payment script and a KSN1 data output carrying the "claim" operation (`claim(lock)`, signed by a one-time key,
//!    nonce 0). Once BTC confirms it, the operation is force-included; this contract reads the transaction ID and all
//!    outputs of that transaction through the host function `native_tx` and computes the KSA from the amount paid to the
//!    payment script and the price.
//! 4. **Redeem**: when `claim` succeeds, the KSA the payment buys is credited directly to the beneficiary account stated at
//!    lock time (the claim operation carries no beneficiary parameter, so copying or replaying it cannot change where the
//!    KSA goes). A payment for less than all of the locked KSA leaves the rest locked for the beneficiary; once all of it
//!    has been bought, the lock becomes "claimed". The buyer then uses the KSA via path B.
//!
//! [`RELEASE_DELAY`] BTC blocks after the expiry, a lock that has still not been claimed can be taken back by the seller
//! with `release` (the KSA returns to the free balance). This delay is for a buyer who pays at the last moment: the claim
//! operation is force-included once the payment transaction has `NATIVE_CONFIRMATIONS` confirmations and a fast-lane block
//! anchors there. Within a block, the forced-inclusion entries come before the block's own entries, but the entries of the
//! blocks it merges execute before them (D-063, with this block's environment): if the block whose anchor height jumps
//! straight to `expiry + RELEASE_DELAY` also merges a side-chain block carrying a `release`, the release executes first. So
//! the buyer should have the payment confirmed in a BTC block before `expiry + RELEASE_DELAY − NATIVE_CONFIRMATIONS − 1`
//! (the `payBy` returned by the SDK's `buildTopUp`); the claim is then force-included at an anchor height before the release
//! becomes possible.
//!
//! The claim operation and the payment are in the same BTC transaction, and the signature of a path C operation is bound to
//! the first input of its carrying transaction (a protocol rule, D-086): whoever copies the signed claim operation into
//! another transaction that pays nothing cannot obtain a valid signature; the operation in that transaction is recorded as
//! failed and does not consume the one-time key's nonce 0, and the buyer's real payment transaction claims as usual. The
//! contract itself does not need to (and cannot) tell whether an operation was copied.
//!
//! Anyone can claim with a claim operation of their own (signed by any key) in a transaction that pays the payment script,
//! and the KSA still goes to the beneficiary. So a claim never takes KSA away from the beneficiary: a partial payment only
//! buys its share and leaves the lock open (D-098). Before M26 a partial payment closed the lock and returned the rest to
//! the seller, which let the seller front-run the buyer's payment with a few sats paid to its own payment script, take the
//! rest of the KSA back and keep the buyer's BTC.
//!
//! Parameters and return values are Borsh-encoded. The parameters form a tuple in the order of the table below (a single
//! parameter is encoded as itself; no parameters means empty bytes):
//!
//! | Method | Parameters | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | none | `()` |
//! | `deposit` | none, KSA attached | `u128` (free balance) |
//! | `withdraw` | `amount: u128` | `u128` (remaining free balance) |
//! | `lock` | `(ksa: u128, price: u128, script: Vec<u8>, beneficiary: [u8; 32], expiry: u32)`, KSA may be attached | `u64` (lock id) |
//! | `claim` | `lock: u64` | `(paid_sats: u64, ksa_paid: u128)` |
//! | `release` | `lock: u64` | `u128` (the KSA taken back) |
//! | `get_lock` | `lock: u64` | [`Lock`] |
//! | `lock_of_script` | `script: Vec<u8>` | `Option<u64>` |
//! | `balance` | `seller: [u8; 32]` | `u128` (free balance) |
//! | `lock_count` | `seller: [u8; 32]` | `u64` (number of locks this seller has created) |
//! | `locks_of` | `(seller: [u8; 32], start: u64, limit: u32)` | `Vec<u64>` (at most `limit` lock ids of this seller, from the `start`-th on) |
//!
//! Rules:
//!
//! - `deposit`: the caller is the seller; the attached KSA (must be greater than 0; no other asset may be attached) goes
//!   into the seller's free balance. `withdraw`: takes KSA back from the free balance, takes effect immediately and does
//!   not affect any lock.
//! - `lock`: the caller is the seller. `ksa > 0`, `price > 0`; the payment script is 1 to [`MAX_SCRIPT_BYTES`] bytes, is
//!   not `OP_RETURN`, and **has never been used by any lock** (claimed and released locks keep it taken forever, so one
//!   payment never corresponds to two locks); `expiry` must be above the BTC height anchored by the current block and at
//!   most `u32::MAX − RELEASE_DELAY`; the beneficiary account must not be this contract itself (nobody can take out KSA
//!   credited to the contract). The attached KSA first goes into the free balance, then `ksa` is moved out of the free
//!   balance into the lock; revert when it does not suffice. The terms of a lock cannot be changed.
//! - `claim`: can only be called by a native (path C) operation (revert when `native_tx` is empty); the lock must be
//!   `Open`. `paid_sats` = the sum of the output amounts the transaction pays to the lock's payment script; revert when it
//!   is 0, and when this transaction has already claimed this lock (a payment counts once).
//!   `ksa_paid = min(paid_sats × price, ksa)` (saturating multiplication, `ksa` = the KSA still locked) is credited to
//!   the beneficiary account and subtracted from the lock's `ksa`. When nothing is left the lock becomes `Claimed` with
//!   the totals of all its claims; otherwise it stays `Open` and the rest stays reserved for the beneficiary.
//!   Overpaying does not buy more KSA; paying into a lock that is not `Open` fails the claim while the BTC has already been
//!   paid, so a wallet must verify the lock's state and expiry height before paying (the SDK's `buildTopUp` checks). A
//!   payment after the lock's expiry but before the seller releases is still valid.
//! - `release`: seller only; the lock must be `Open` and `btc_anchor_height() ≥ expiry + RELEASE_DELAY`. The KSA still
//!   locked returns to the seller's free balance and the lock becomes `Released`.
//!
//! Storage is updated first, transfers come last (checks-effects-interactions).
//!
//! Events (Borsh): `("deposit", seller, amount, balance)`, `("withdraw", seller, amount, balance)`,
//! `("lock", lock, seller, beneficiary, price, ksa, expiry)`, `("claim", lock, beneficiary, txid, paid_sats, ksa_paid, remaining)`
//! (`remaining`: the KSA still locked after this claim), `("release", lock, ksa)`.

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::vec::Vec;

use ksana_contract_sdk::borsh::BorshSerialize;
use ksana_contract_sdk::{self as sdk, storage, CallResult, Decode, KSA_ASSET_ID};

sdk::entry!(dispatch);

/// Maximum length of a payment script in bytes.
pub const MAX_SCRIPT_BYTES: usize = 64;
/// How many BTC blocks after the expiry the seller has to wait before releasing a lock: time for the claim of a buyer
/// who pays at the last moment to be force-included.
pub const RELEASE_DELAY: u32 = 6;
/// Maximum number of lock ids `locks_of` returns per call.
pub const MAX_LOCKS_PER_QUERY: u32 = 256;

const KEY_NEXT_LOCK: &[u8] = b"next";
const PREFIX_LOCK: &[u8] = b"l";
const PREFIX_SCRIPT: &[u8] = b"s";
const PREFIX_BALANCE: &[u8] = b"b";
const PREFIX_COUNT: &[u8] = b"n";
const PREFIX_SELLER_LOCK: &[u8] = b"k";
/// The totals `(paid_sats, ksa_paid)` of the claims of a lock that is still open after a partial claim.
const PREFIX_FILLED: &[u8] = b"f";
/// The BTC transactions that have claimed a lock: (lock, txid) → the sats it paid (`u64`).
const PREFIX_CLAIM_TX: &[u8] = b"t";

const OP_RETURN: u8 = 0x6a;

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_INIT_FUNDS: &[u8] = b"init takes no funds";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_ONLY_KSA: &[u8] = b"only KSA may be attached";
const ERR_NO_FUNDS: &[u8] = b"attach KSA";
const ERR_ZERO_AMOUNT: &[u8] = b"amount must be positive";
const ERR_INSUFFICIENT_BALANCE: &[u8] = b"insufficient free balance";
const ERR_ZERO_KSA: &[u8] = b"ksa must be positive";
const ERR_ZERO_PRICE: &[u8] = b"price must be positive";
const ERR_BAD_SCRIPT: &[u8] = b"script must be 1 to 64 bytes and not OP_RETURN";
const ERR_SCRIPT_TAKEN: &[u8] = b"script already used by a lock";
const ERR_BAD_BENEFICIARY: &[u8] = b"beneficiary must not be the exchange itself";
const ERR_EXPIRY: &[u8] = b"expiry must be above the anchored BTC height";
const ERR_EXPIRY_TOO_LARGE: &[u8] = b"expiry too large";
const ERR_UNKNOWN_LOCK: &[u8] = b"unknown lock";
const ERR_NOT_SELLER: &[u8] = b"only the seller";
const ERR_NOT_OPEN: &[u8] = b"lock is not open";
const ERR_NOT_NATIVE: &[u8] = b"claim must be a native (path C) operation";
const ERR_NO_PAYMENT: &[u8] = b"the transaction pays nothing to this lock";
const ERR_TX_USED: &[u8] = b"this transaction has already claimed this lock";
const ERR_TOO_EARLY: &[u8] = b"release delay has not passed";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";
const ERR_LIMIT: &[u8] = b"limit must be 1 to 256";
const ERR_MISSING_INDEX: &[u8] = b"missing lock index";

/// State of a lock (Borsh: a `u8` variant index; for `Claimed` it is followed by `paid_sats: u64` and `ksa_paid: u128`).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub enum LockState {
    /// Waiting for payments: claims can still buy the KSA that is still locked ([`Lock::ksa`]).
    Open,
    /// Claimed: all of the locked KSA has been bought; the claims paid `paid_sats` sats in total and the beneficiary account
    /// received `ksa_paid` in total.
    Claimed {
        /// Total amount paid to the payment script by the claims (sats).
        paid_sats: u64,
        /// Total KSA credited to the beneficiary account.
        ksa_paid: u128,
    },
    /// Released by the seller after the expiry; the KSA that was still locked has returned to the seller's free balance.
    Released,
}

impl Decode for LockState {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(LockState::Open),
            1 => Some(LockState::Claimed {
                paid_sats: u64::decode(input)?,
                ksa_paid: u128::decode(input)?,
            }),
            2 => Some(LockState::Released),
            _ => None,
        }
    }
}

/// A lock (the value in storage and the return value of `get_lock`; the Borsh field order is as listed).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Lock {
    /// Seller account.
    pub seller: [u8; 32],
    /// Beneficiary account: the buyer account that receives the KSA on claim, fixed at lock time.
    pub beneficiary: [u8; 32],
    /// BTC payment script, used by this one lock only.
    pub script: Vec<u8>,
    /// Price: how many KSA base units per sat.
    pub price: u128,
    /// KSA still locked: the amount locked minus what claims have bought so far (0 once claimed; for a released lock, the
    /// amount released).
    pub ksa: u128,
    /// Expiry BTC height; [`RELEASE_DELAY`] blocks later the seller can release.
    pub expiry: u32,
    /// State.
    pub state: LockState,
}

impl Decode for Lock {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            seller: <[u8; 32]>::decode(input)?,
            beneficiary: <[u8; 32]>::decode(input)?,
            script: Vec::<u8>::decode(input)?,
            price: u128::decode(input)?,
            ksa: u128::decode(input)?,
            expiry: u32::decode(input)?,
            state: LockState::decode(input)?,
        })
    }
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            sdk::decode::<()>(args)?;
            init()?;
            sdk::ok(&())
        }
        "deposit" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&deposit()?)
        }
        "withdraw" => {
            let amount = sdk::decode(args)?;
            sdk::ok(&withdraw(amount)?)
        }
        "lock" => {
            let (ksa, price, script, beneficiary, expiry) = sdk::decode(args)?;
            sdk::ok(&lock(ksa, price, script, beneficiary, expiry)?)
        }
        "claim" => {
            let id = sdk::decode(args)?;
            sdk::ok(&claim(id)?)
        }
        "release" => {
            let id = sdk::decode(args)?;
            sdk::ok(&release(id)?)
        }
        "get_lock" => {
            let id = sdk::decode(args)?;
            sdk::ok(&load(id)?)
        }
        "lock_of_script" => {
            let script: Vec<u8> = sdk::decode(args)?;
            sdk::ok(&lock_of_script(&script)?)
        }
        "balance" => {
            let seller: [u8; 32] = sdk::decode(args)?;
            sdk::ok(&free_balance(&seller)?)
        }
        "lock_count" => {
            let seller: [u8; 32] = sdk::decode(args)?;
            sdk::ok(&lock_count(&seller)?)
        }
        "locks_of" => {
            let (seller, start, limit): ([u8; 32], u64, u32) = sdk::decode(args)?;
            sdk::ok(&locks_of(&seller, start, limit)?)
        }
        _ => sdk::revert("unknown method"),
    }
}

fn err(reason: &[u8]) -> Vec<u8> {
    reason.to_vec()
}

fn lock_key(id: u64) -> Vec<u8> {
    [PREFIX_LOCK, id.to_be_bytes().as_slice()].concat()
}

fn script_key(script: &[u8]) -> Vec<u8> {
    [PREFIX_SCRIPT, sdk::sha256(script).as_slice()].concat()
}

fn balance_key(seller: &[u8; 32]) -> Vec<u8> {
    [PREFIX_BALANCE, seller.as_slice()].concat()
}

fn count_key(seller: &[u8; 32]) -> Vec<u8> {
    [PREFIX_COUNT, seller.as_slice()].concat()
}

fn filled_key(id: u64) -> Vec<u8> {
    [PREFIX_FILLED, id.to_be_bytes().as_slice()].concat()
}

fn claim_tx_key(id: u64, txid: &[u8; 32]) -> Vec<u8> {
    [
        PREFIX_CLAIM_TX,
        id.to_be_bytes().as_slice(),
        txid.as_slice(),
    ]
    .concat()
}

fn seller_lock_key(seller: &[u8; 32], index: u64) -> Vec<u8> {
    [
        PREFIX_SELLER_LOCK,
        seller.as_slice(),
        index.to_be_bytes().as_slice(),
    ]
    .concat()
}

/// KSA attached to this call; reverts when any other asset is attached.
fn attached_ksa() -> Result<u128, Vec<u8>> {
    let mut amount = 0u128;
    for funds in sdk::call_funds() {
        if funds.asset != KSA_ASSET_ID {
            return Err(err(ERR_ONLY_KSA));
        }
        amount = funds.amount;
    }
    Ok(amount)
}

fn load(id: u64) -> Result<Lock, Vec<u8>> {
    storage::read(&lock_key(id))?.ok_or_else(|| err(ERR_UNKNOWN_LOCK))
}

fn save(id: u64, lock: &Lock) {
    storage::write(&lock_key(id), lock);
}

/// The seller's free balance (0 when there is no record).
fn free_balance(seller: &[u8; 32]) -> Result<u128, Vec<u8>> {
    Ok(storage::read(&balance_key(seller))?.unwrap_or(0))
}

/// Writes the seller's free balance; removes the record when it is 0.
fn set_free_balance(seller: &[u8; 32], amount: u128) {
    let key = balance_key(seller);
    if amount == 0 {
        storage::remove(&key);
    } else {
        storage::write(&key, &amount);
    }
}

/// Adds `amount` to the free balance and returns the new balance.
fn credit(seller: &[u8; 32], amount: u128) -> Result<u128, Vec<u8>> {
    let balance = free_balance(seller)?
        .checked_add(amount)
        .ok_or_else(|| err(ERR_OVERFLOW))?;
    set_free_balance(seller, balance);
    Ok(balance)
}

/// Number of locks the seller has created.
fn lock_count(seller: &[u8; 32]) -> Result<u64, Vec<u8>> {
    Ok(storage::read(&count_key(seller))?.unwrap_or(0))
}

/// Appends lock `id` to the seller's list of locks.
fn push_seller_lock(seller: &[u8; 32], id: u64) -> Result<(), Vec<u8>> {
    let count = lock_count(seller)?;
    let next = count.checked_add(1).ok_or_else(|| err(ERR_OVERFLOW))?;
    storage::write(&seller_lock_key(seller, count), &id);
    storage::write(&count_key(seller), &next);
    Ok(())
}

/// `init()`.
pub fn init() -> Result<(), Vec<u8>> {
    if storage::get(KEY_NEXT_LOCK).is_some() {
        return Err(err(ERR_ALREADY_INITIALIZED));
    }
    if attached_ksa()? != 0 {
        return Err(err(ERR_INIT_FUNDS));
    }
    storage::write(KEY_NEXT_LOCK, &0u64);
    Ok(())
}

/// `deposit() -> balance`: the caller deposits the attached KSA into the free balance.
pub fn deposit() -> Result<u128, Vec<u8>> {
    let amount = attached_ksa()?;
    if amount == 0 {
        return Err(err(ERR_NO_FUNDS));
    }
    let seller = sdk::caller();
    let balance = credit(&seller, amount)?;
    sdk::emit(&("deposit", seller, amount, balance));
    Ok(balance)
}

/// `withdraw(amount) -> balance`: the caller takes KSA back from the free balance; no lock is affected.
pub fn withdraw(amount: u128) -> Result<u128, Vec<u8>> {
    if amount == 0 {
        return Err(err(ERR_ZERO_AMOUNT));
    }
    let seller = sdk::caller();
    let balance = free_balance(&seller)?
        .checked_sub(amount)
        .ok_or_else(|| err(ERR_INSUFFICIENT_BALANCE))?;
    set_free_balance(&seller, balance);
    sdk::asset_transfer(&KSA_ASSET_ID, &seller, amount);
    sdk::emit(&("withdraw", seller, amount, balance));
    Ok(balance)
}

/// `lock(ksa, price, script, beneficiary, expiry) -> lock`: the caller (the seller) moves `ksa` from the free balance (the
/// attached KSA is counted in first) into a new lock.
pub fn lock(
    ksa: u128,
    price: u128,
    script: Vec<u8>,
    beneficiary: [u8; 32],
    expiry: u32,
) -> Result<u64, Vec<u8>> {
    let id: u64 = storage::read(KEY_NEXT_LOCK)?.ok_or_else(|| err(ERR_NOT_INITIALIZED))?;
    if ksa == 0 {
        return Err(err(ERR_ZERO_KSA));
    }
    if price == 0 {
        return Err(err(ERR_ZERO_PRICE));
    }
    if script.is_empty() || script.len() > MAX_SCRIPT_BYTES || script.first() == Some(&OP_RETURN) {
        return Err(err(ERR_BAD_SCRIPT));
    }
    if expiry <= sdk::btc_anchor_height() {
        return Err(err(ERR_EXPIRY));
    }
    if expiry > u32::MAX - RELEASE_DELAY {
        return Err(err(ERR_EXPIRY_TOO_LARGE));
    }
    // nobody can take out KSA credited to the contract itself: a seller who gets the beneficiary account wrong would trap
    // that KSA for nothing
    if beneficiary == sdk::self_id() {
        return Err(err(ERR_BAD_BENEFICIARY));
    }
    let index = script_key(&script);
    if storage::get(&index).is_some() {
        return Err(err(ERR_SCRIPT_TAKEN));
    }
    let seller = sdk::caller();
    let attached = attached_ksa()?;
    let available = free_balance(&seller)?
        .checked_add(attached)
        .ok_or_else(|| err(ERR_OVERFLOW))?;
    let remaining = available
        .checked_sub(ksa)
        .ok_or_else(|| err(ERR_INSUFFICIENT_BALANCE))?;
    let next = id.checked_add(1).ok_or_else(|| err(ERR_OVERFLOW))?;
    set_free_balance(&seller, remaining);
    storage::write(&index, &id);
    storage::write(KEY_NEXT_LOCK, &next);
    push_seller_lock(&seller, id)?;
    save(
        id,
        &Lock {
            seller,
            beneficiary,
            script,
            price,
            ksa,
            expiry,
            state: LockState::Open,
        },
    );
    if attached > 0 {
        sdk::emit(&("deposit", seller, attached, available));
    }
    sdk::emit(&("lock", id, seller, beneficiary, price, ksa, expiry));
    Ok(id)
}

/// `claim(lock) -> (paid_sats, ksa_paid)`: credits the lock's beneficiary account with the KSA that the payment to the lock's
/// payment script in the BTC transaction carrying this operation buys at the lock's price, at most the KSA still locked; the
/// lock stays open until all of its KSA has been bought.
pub fn claim(id: u64) -> Result<(u64, u128), Vec<u8>> {
    let tx = sdk::native_tx().ok_or_else(|| err(ERR_NOT_NATIVE))?;
    let mut lock = load(id)?;
    if lock.state != LockState::Open {
        return Err(err(ERR_NOT_OPEN));
    }
    let mut paid_sats = 0u64;
    for output in tx.outputs.iter().filter(|o| o.script == lock.script) {
        paid_sats = paid_sats
            .checked_add(output.value)
            .ok_or_else(|| err(ERR_OVERFLOW))?;
    }
    if paid_sats == 0 {
        return Err(err(ERR_NO_PAYMENT));
    }
    // a payment counts once: the same transaction cannot claim again (e.g. a contract calling `claim` twice in one
    // operation), now that a partial claim leaves the lock open
    let claim_tx = claim_tx_key(id, &tx.txid);
    if storage::get(&claim_tx).is_some() {
        return Err(err(ERR_TX_USED));
    }
    storage::write(&claim_tx, &paid_sats);
    let ksa_paid = u128::from(paid_sats)
        .saturating_mul(lock.price)
        .min(lock.ksa);
    let remaining = lock
        .ksa
        .checked_sub(ksa_paid)
        .ok_or_else(|| err(ERR_OVERFLOW))?;
    // the totals of this lock's claims, this one included
    let filled = filled_key(id);
    let (sats_before, ksa_before): (u64, u128) = storage::read(&filled)?.unwrap_or((0, 0));
    let total_sats = sats_before
        .checked_add(paid_sats)
        .ok_or_else(|| err(ERR_OVERFLOW))?;
    let total_ksa = ksa_before
        .checked_add(ksa_paid)
        .ok_or_else(|| err(ERR_OVERFLOW))?;
    lock.ksa = remaining;
    if remaining == 0 {
        lock.state = LockState::Claimed {
            paid_sats: total_sats,
            ksa_paid: total_ksa,
        };
        storage::remove(&filled);
    } else {
        storage::write(&filled, &(total_sats, total_ksa));
    }
    save(id, &lock);
    if ksa_paid > 0 {
        sdk::asset_transfer(&KSA_ASSET_ID, &lock.beneficiary, ksa_paid);
    }
    sdk::emit(&(
        "claim",
        id,
        lock.beneficiary,
        tx.txid,
        paid_sats,
        ksa_paid,
        remaining,
    ));
    Ok((paid_sats, ksa_paid))
}

/// `release(lock) -> ksa`: once the expiry plus [`RELEASE_DELAY`] has passed, the seller takes back the KSA still locked by a
/// lock that is still open (it returns to the free balance).
pub fn release(id: u64) -> Result<u128, Vec<u8>> {
    let mut lock = load(id)?;
    if sdk::caller() != lock.seller {
        return Err(err(ERR_NOT_SELLER));
    }
    if lock.state != LockState::Open {
        return Err(err(ERR_NOT_OPEN));
    }
    let releasable = u64::from(lock.expiry).saturating_add(u64::from(RELEASE_DELAY));
    if u64::from(sdk::btc_anchor_height()) < releasable {
        return Err(err(ERR_TOO_EARLY));
    }
    let ksa = lock.ksa;
    lock.state = LockState::Released;
    save(id, &lock);
    storage::remove(&filled_key(id));
    credit(&lock.seller, ksa)?;
    sdk::emit(&("release", id, ksa));
    Ok(ksa)
}

/// `lock_of_script(script) -> Option<lock>`: the lock that has used this payment script.
pub fn lock_of_script(script: &[u8]) -> Result<Option<u64>, Vec<u8>> {
    storage::read(&script_key(script))
}

/// `locks_of(seller, start, limit)`: the seller's lock ids from the `start`-th on, at most `limit` of them.
pub fn locks_of(seller: &[u8; 32], start: u64, limit: u32) -> Result<Vec<u64>, Vec<u8>> {
    if limit == 0 || limit > MAX_LOCKS_PER_QUERY {
        return Err(err(ERR_LIMIT));
    }
    let count = lock_count(seller)?;
    let to = start.saturating_add(u64::from(limit)).min(count);
    let mut out = Vec::new();
    let mut index = start;
    while index < to {
        let id: u64 = storage::read(&seller_lock_key(seller, index))?
            .ok_or_else(|| err(ERR_MISSING_INDEX))?;
        out.push(id);
        index = index.checked_add(1).ok_or_else(|| err(ERR_OVERFLOW))?;
    }
    Ok(out)
}
