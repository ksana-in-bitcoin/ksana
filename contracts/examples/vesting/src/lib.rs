//! Example contract: vesting schedules. Each schedule pays an amount of one built-in asset to its beneficiary,
//! linearly over time after a cliff; a revocable one can be stopped by its creator, who takes back what has not vested.
//!
//! One contract holds any number of schedules. Anyone creates one by attaching an amount of one built-in asset (KSA,
//! or an asset a contract issued with `asset_issue`, D-073) for a beneficiary. Anyone can release what has vested,
//! which is always paid to the beneficiary. The creator of a revocable schedule can revoke it and take back the part
//! that has not vested; what has vested stays the beneficiary's.
//!
//! Arguments and return values are Borsh-encoded. The arguments are a tuple in the order of the table below (a single
//! argument is just itself; no arguments is empty bytes):
//!
//! | Method | Arguments | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | none | `()` |
//! | `create` (the amount attached) | `(beneficiary: [u8; 32], start: u64, cliff: u64, duration: u64, revocable: bool)` | `u64` (the new schedule's ID) |
//! | `release` | `id: u64` | `u128` (the amount paid to the beneficiary) |
//! | `revoke` | `id: u64` | `u128` (the amount refunded to the creator) |
//! | `schedule` | `id: u64` | `Option<Schedule>` |
//! | `releasable` | `id: u64` | `u128` |
//! | `vested` | `(id: u64, at: u64)` | `u128` |
//! | `count` | none | `u64` |
//! | `by_beneficiary` | `(account: [u8; 32], start: u64, limit: u32)` | `Vec<Schedule>` |
//! | `by_creator` | `(account: [u8; 32], start: u64, limit: u32)` | `Vec<Schedule>` |
//!
//! Time is in Unix seconds and comes from the block's timestamp, which the block's producer sets and which is only
//! roughly accurate (block heights would be worse: an idle chain produces blocks minutes apart). The contract keeps a
//! clock, the latest timestamp seen by a call that changed its state (`init`, `create`, `release`, `revoke`), and its
//! `now` is the later of the clock and the block's timestamp: a block whose timestamp is not after the clock counts as
//! no time having passed, so what has vested never shrinks and nothing is ever subtracted into a negative.
//!
//! - `create`: attach exactly one asset; all of it vests. The beneficiary must not be the zero account or this
//!   contract. `start` 0 means `now`; any other start may be in the past or in the future, at most 100 years after
//!   `now` (a start in milliseconds would lock the amount for ages). `cliff` and `duration` are seconds counted from
//!   `start`, with `cliff ≤ duration` and `0 < duration ≤ 3 153 600 000` (100 years). IDs count from 1. Each schedule
//!   is appended to its beneficiary's list and to its creator's list, which `by_beneficiary` and `by_creator` page by
//!   position in creation order (at most [`MAX_PAGE`] at a time; a start past the end gives an empty list).
//! - Vesting: nothing before `start + cliff`; everything from `start + duration` on; in between
//!   `floor(total · (t − start) / duration)`, linear from `start` (the cliff only holds it back). The product is exact
//!   for any total, up to `u128::MAX` ([`math::mul_div`]).
//! - `release`: anyone may call it. It pays the beneficiary what has vested by `now` and has not been released yet,
//!   and reverts when that is 0.
//! - `revoke`: only the creator, of a revocable schedule that is not revoked yet. What has vested by `now` stays the
//!   beneficiary's: the schedule's `total` becomes that amount, `revoked_at` becomes `now`, and the rest is refunded
//!   to the creator (a refund of 0 transfers nothing, and the schedule is revoked all the same). A revoked schedule has
//!   vested all of its reduced total, which the beneficiary releases as before.
//! - `releasable(id)` is what `release(id)` would pay now. `vested(id, at)` applies the schedule to any time `at`,
//!   without the contract's clock (to draw its curve); for a revoked schedule it is the reduced total whatever `at` is.
//! - Only `create` takes attached funds; every other method refuses them, as they would be stuck in the contract. The
//!   views only read.
//!
//! Of each asset, the contract holds exactly the sum of `total − released` over the schedules in that asset. Storage is
//! written before an asset is transferred (checks-effects-interactions).
//!
//! Events (Borsh): `("create", id, creator, beneficiary, asset, amount)`, `("release", id, beneficiary, amount)`,
//! `("revoke", id, refunded)`.
//!
//! ```abi
//! struct Schedule { id: u64, creator: Account, beneficiary: Account, asset: AssetId, total: u128, released: u128, start: u64, cliff: u64, duration: u64, revocable: bool, revoked_at: Option<u64> }
//! init()
//! payable call create(beneficiary: Account, start: u64, cliff: u64, duration: u64, revocable: bool) -> u64  // attach the amount to vest; start 0 means now
//! call release(id: u64) -> u128                       // pays what has vested to the beneficiary; anyone may call
//! call revoke(id: u64) -> u128                        // the creator, if revocable: refunds what has not vested yet
//! view schedule(id: u64) -> Option<Schedule>          // None when no schedule has this ID
//! view releasable(id: u64) -> u128                    // what release would pay now
//! view vested(id: u64, at: u64) -> u128               // what has vested by the time `at` (Unix seconds)
//! view count() -> u64                                 // schedules created so far; IDs run from 1 to count
//! view by_beneficiary(account: Account, start: u64, limit: u32) -> Vec<Schedule>  // the account's schedules from position start, at most 50
//! view by_creator(account: Account, start: u64, limit: u32) -> Vec<Schedule>      // the schedules the account created, likewise
//! event create(id: u64, creator: Account, beneficiary: Account, asset: AssetId, amount: u128)
//! event release(id: u64, beneficiary: Account, amount: u128)
//! event revoke(id: u64, refunded: u128)
//! ```

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::vec::Vec;

use ksana_contract_sdk::borsh::BorshSerialize;
use ksana_contract_sdk::{self as sdk, storage, CallResult, Decode, Funds};

sdk::entry!(dispatch);

/// The longest `duration`: 100 years of 365 days, in seconds.
pub const MAX_DURATION: u64 = 3_153_600_000;
/// The most schedules `by_beneficiary` and `by_creator` return per call.
pub const MAX_PAGE: u32 = 50;

/// How many schedules have been created (`u64`), which is also the latest ID; written by `init`.
const KEY_COUNT: &[u8] = b"count";
/// The contract's clock: the latest block timestamp a call has kept (`u64`, Unix seconds); written by `init`.
const KEY_CLOCK: &[u8] = b"clock";
/// `s ‖ id` (u64 big-endian) → [`Schedule`].
const PREFIX_SCHEDULE: &[u8] = b"s";

/// The schedules of each beneficiary.
const BENEFICIARY_LIST: List = List {
    len: b"bn",
    item: b"bi",
};
/// The schedules each creator created.
const CREATOR_LIST: List = List {
    len: b"cn",
    item: b"ci",
};

const ZERO_ACCOUNT: [u8; 32] = [0u8; 32];

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_UNEXPECTED_FUNDS: &[u8] = b"unexpected attached funds";
const ERR_ATTACH_ONE: &[u8] = b"attach one asset to vest";
const ERR_ZERO_BENEFICIARY: &[u8] = b"beneficiary must not be zero";
const ERR_SELF_BENEFICIARY: &[u8] = b"beneficiary must not be this contract";
const ERR_DURATION: &[u8] = b"duration must be 1 to 3153600000 seconds";
const ERR_CLIFF: &[u8] = b"cliff must not exceed duration";
const ERR_START: &[u8] = b"start must be at most 100 years from now";
const ERR_UNKNOWN_SCHEDULE: &[u8] = b"unknown schedule";
const ERR_NOTHING_TO_RELEASE: &[u8] = b"nothing to release";
const ERR_NOT_CREATOR: &[u8] = b"only the creator";
const ERR_NOT_REVOCABLE: &[u8] = b"schedule is not revocable";
const ERR_ALREADY_REVOKED: &[u8] = b"already revoked";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";
const ERR_MISSING_INDEX: &[u8] = b"missing schedule index";

/// A vesting schedule: the value in storage and what the views return (the Borsh field order is as listed).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Schedule {
    /// The schedule's ID, counting from 1.
    pub id: u64,
    /// The account that created it and attached the amount; the only one that may revoke it.
    pub creator: [u8; 32],
    /// The account every release pays.
    pub beneficiary: [u8; 32],
    /// The built-in asset that vests ([`sdk::KSA_ASSET_ID`] for KSA).
    pub asset: [u8; 32],
    /// The amount that vests: what was attached, or, once revoked, what had vested by then.
    pub total: u128,
    /// What has been paid to the beneficiary so far, never more than `total`.
    pub released: u128,
    /// When vesting starts (Unix seconds).
    pub start: u64,
    /// Seconds after `start` before which nothing vests.
    pub cliff: u64,
    /// Seconds after `start` by which everything has vested.
    pub duration: u64,
    /// Whether the creator may revoke it.
    pub revocable: bool,
    /// When it was revoked (the contract's `now` then); `None` while it is not.
    pub revoked_at: Option<u64>,
}

impl Decode for Schedule {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            id: u64::decode(input)?,
            creator: <[u8; 32]>::decode(input)?,
            beneficiary: <[u8; 32]>::decode(input)?,
            asset: <[u8; 32]>::decode(input)?,
            total: u128::decode(input)?,
            released: u128::decode(input)?,
            start: u64::decode(input)?,
            cliff: u64::decode(input)?,
            duration: u64::decode(input)?,
            revocable: bool::decode(input)?,
            revoked_at: Option::<u64>::decode(input)?,
        })
    }
}

impl Schedule {
    /// What has vested by the time `at` (Unix seconds): nothing before `start + cliff`, everything from
    /// `start + duration` on, `floor(total · (at − start) / duration)` in between; for a revoked schedule all of its
    /// reduced total, whatever `at` is. `None` only for times that overflow, which `create` never stores.
    pub fn vested_at(&self, at: u64) -> Option<u128> {
        if self.revoked_at.is_some() {
            return Some(self.total);
        }
        if at < self.start.checked_add(self.cliff)? {
            return Some(0);
        }
        let elapsed = at.checked_sub(self.start)?;
        if elapsed >= self.duration {
            return Some(self.total);
        }
        math::mul_div(self.total, elapsed, self.duration)
    }

    /// What a release at the time `now` pays: what has vested by then and has not been released yet.
    pub fn releasable(&self, now: u64) -> Option<u128> {
        Some(self.vested_at(now)?.saturating_sub(self.released))
    }
}

/// Pure integer arithmetic (no host access).
pub mod math {
    /// `floor(value · numerator / denominator)`, exact for every `value`: with `value = q · denominator + r`, it is
    /// `q · numerator + floor(r · numerator / denominator)`, where `r · numerator` fits because both factors are below
    /// 2^64. `None` when `denominator` is 0 or the result exceeds `u128::MAX`, which it cannot when
    /// `numerator ≤ denominator`.
    pub fn mul_div(value: u128, numerator: u64, denominator: u64) -> Option<u128> {
        let (numerator, denominator) = (u128::from(numerator), u128::from(denominator));
        let whole = value.checked_div(denominator)?.checked_mul(numerator)?;
        let part = value
            .checked_rem(denominator)?
            .checked_mul(numerator)?
            .checked_div(denominator)?;
        whole.checked_add(part)
    }
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    // Only `create` is payable: funds attached to any other call would be stuck in the contract
    if method != "create" && !sdk::call_funds().is_empty() {
        return Err(err(ERR_UNEXPECTED_FUNDS));
    }
    match method {
        "init" => {
            sdk::decode::<()>(args)?;
            init()?;
            sdk::ok(&())
        }
        "create" => {
            let (beneficiary, start, cliff, duration, revocable) = sdk::decode(args)?;
            sdk::ok(&create(beneficiary, start, cliff, duration, revocable)?)
        }
        "release" => sdk::ok(&release(sdk::decode(args)?)?),
        "revoke" => sdk::ok(&revoke(sdk::decode(args)?)?),
        "schedule" => sdk::ok(&schedule(sdk::decode(args)?)?),
        "releasable" => sdk::ok(&releasable(sdk::decode(args)?)?),
        "vested" => {
            let (id, at) = sdk::decode(args)?;
            sdk::ok(&vested(id, at)?)
        }
        "count" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&count()?)
        }
        "by_beneficiary" => {
            let (account, start, limit) = sdk::decode(args)?;
            sdk::ok(&by_beneficiary(&account, start, limit)?)
        }
        "by_creator" => {
            let (account, start, limit) = sdk::decode(args)?;
            sdk::ok(&by_creator(&account, start, limit)?)
        }
        _ => sdk::revert("unknown method"),
    }
}

fn err(reason: &[u8]) -> Vec<u8> {
    reason.to_vec()
}

fn overflow() -> Vec<u8> {
    err(ERR_OVERFLOW)
}

fn schedule_key(id: u64) -> Vec<u8> {
    [PREFIX_SCHEDULE, id.to_be_bytes().as_slice()].concat()
}

fn save(schedule: &Schedule) {
    storage::write(&schedule_key(schedule.id), schedule);
}

/// The schedule with this ID; reverts when there is none.
fn load(id: u64) -> Result<Schedule, Vec<u8>> {
    match storage::read(&schedule_key(id))? {
        Some(schedule) => Ok(schedule),
        None => {
            // Before `init`, say so
            count()?;
            Err(err(ERR_UNKNOWN_SCHEDULE))
        }
    }
}

/// The contract's time.
#[derive(Clone, Copy)]
struct Clock {
    /// The latest block timestamp a call has kept.
    kept: u64,
    /// The later of `kept` and this block's timestamp, so that the contract's time never runs backwards.
    now: u64,
}

impl Clock {
    fn read() -> Result<Self, Vec<u8>> {
        let kept: u64 = storage::read(KEY_CLOCK)?.ok_or_else(|| err(ERR_NOT_INITIALIZED))?;
        Ok(Self {
            kept,
            now: kept.max(sdk::block_timestamp()),
        })
    }

    /// Keeps `now` for the calls that follow; the methods that change state do so with their other effects.
    fn keep(self) {
        if self.now > self.kept {
            storage::write(KEY_CLOCK, &self.now);
        }
    }
}

/// A list of schedule IDs per account, in creation order: `len ‖ account` holds its length (`u64`, absent while 0)
/// and `item ‖ account ‖ position` (u64 big-endian) the ID at that position.
struct List {
    len: &'static [u8],
    item: &'static [u8],
}

impl List {
    fn len_key(&self, account: &[u8; 32]) -> Vec<u8> {
        [self.len, account.as_slice()].concat()
    }

    fn item_key(&self, account: &[u8; 32], position: u64) -> Vec<u8> {
        [
            self.item,
            account.as_slice(),
            position.to_be_bytes().as_slice(),
        ]
        .concat()
    }

    fn length(&self, account: &[u8; 32]) -> Result<u64, Vec<u8>> {
        Ok(storage::read(&self.len_key(account))?.unwrap_or(0))
    }

    /// Appends schedule `id` to the list of `account`.
    fn push(&self, account: &[u8; 32], id: u64) -> Result<(), Vec<u8>> {
        let position = self.length(account)?;
        let length = position.checked_add(1).ok_or_else(overflow)?;
        storage::write(&self.item_key(account, position), &id);
        storage::write(&self.len_key(account), &length);
        Ok(())
    }

    /// The schedules in the list of `account` from position `start` on: at most `limit`, and at most [`MAX_PAGE`].
    fn page(&self, account: &[u8; 32], start: u64, limit: u32) -> Result<Vec<Schedule>, Vec<u8>> {
        count()?;
        let end = start
            .saturating_add(u64::from(limit.min(MAX_PAGE)))
            .min(self.length(account)?);
        let mut page = Vec::new();
        for position in start..end {
            let id = storage::read(&self.item_key(account, position))?
                .ok_or_else(|| err(ERR_MISSING_INDEX))?;
            page.push(load(id)?);
        }
        Ok(page)
    }
}

/// `init()`.
pub fn init() -> Result<(), Vec<u8>> {
    if storage::get(KEY_COUNT).is_some() {
        return Err(err(ERR_ALREADY_INITIALIZED));
    }
    storage::write(KEY_COUNT, &0u64);
    storage::write(KEY_CLOCK, &sdk::block_timestamp());
    Ok(())
}

/// `create(beneficiary, start, cliff, duration, revocable) -> id`: the caller attaches one asset, which vests for
/// `beneficiary` from `start` (0: now) over `duration` seconds, nothing of it before `cliff` seconds.
pub fn create(
    beneficiary: [u8; 32],
    start: u64,
    cliff: u64,
    duration: u64,
    revocable: bool,
) -> Result<u64, Vec<u8>> {
    let last = count()?;
    let clock = Clock::read()?;
    let funds = sdk::call_funds();
    let &[Funds { asset, amount }] = funds.as_slice() else {
        return Err(err(ERR_ATTACH_ONE));
    };
    if amount == 0 {
        return Err(err(ERR_ATTACH_ONE));
    }
    if beneficiary == ZERO_ACCOUNT {
        return Err(err(ERR_ZERO_BENEFICIARY));
    }
    // A release would pay the contract itself, and the amount would be stuck
    if beneficiary == sdk::self_id() {
        return Err(err(ERR_SELF_BENEFICIARY));
    }
    if duration == 0 || duration > MAX_DURATION {
        return Err(err(ERR_DURATION));
    }
    if cliff > duration {
        return Err(err(ERR_CLIFF));
    }
    let start = if start == 0 { clock.now } else { start };
    if start > clock.now.saturating_add(MAX_DURATION) {
        return Err(err(ERR_START));
    }
    // The end of the schedule must be a time too
    start.checked_add(duration).ok_or_else(overflow)?;
    let id = last.checked_add(1).ok_or_else(overflow)?;
    let creator = sdk::caller();
    clock.keep();
    storage::write(KEY_COUNT, &id);
    save(&Schedule {
        id,
        creator,
        beneficiary,
        asset,
        total: amount,
        released: 0,
        start,
        cliff,
        duration,
        revocable,
        revoked_at: None,
    });
    BENEFICIARY_LIST.push(&beneficiary, id)?;
    CREATOR_LIST.push(&creator, id)?;
    sdk::emit(&("create", id, creator, beneficiary, asset, amount));
    Ok(id)
}

/// `release(id) -> amount`: pays the beneficiary what has vested and has not been released yet; anyone may call it.
pub fn release(id: u64) -> Result<u128, Vec<u8>> {
    let clock = Clock::read()?;
    let mut schedule = load(id)?;
    let amount = schedule.releasable(clock.now).ok_or_else(overflow)?;
    if amount == 0 {
        return Err(err(ERR_NOTHING_TO_RELEASE));
    }
    schedule.released = schedule.released.checked_add(amount).ok_or_else(overflow)?;
    clock.keep();
    save(&schedule);
    sdk::asset_transfer(&schedule.asset, &schedule.beneficiary, amount);
    sdk::emit(&("release", id, schedule.beneficiary, amount));
    Ok(amount)
}

/// `revoke(id) -> refunded`: the creator of a revocable schedule stops it; what has vested stays the beneficiary's
/// and the rest is refunded to the creator.
pub fn revoke(id: u64) -> Result<u128, Vec<u8>> {
    let clock = Clock::read()?;
    let mut schedule = load(id)?;
    if sdk::caller() != schedule.creator {
        return Err(err(ERR_NOT_CREATOR));
    }
    if !schedule.revocable {
        return Err(err(ERR_NOT_REVOCABLE));
    }
    if schedule.revoked_at.is_some() {
        return Err(err(ERR_ALREADY_REVOKED));
    }
    // Never less than what was released (the clock already sees to that), so the refund only takes what the
    // schedule still holds
    let vested = schedule
        .vested_at(clock.now)
        .ok_or_else(overflow)?
        .max(schedule.released);
    let refund = schedule.total.checked_sub(vested).ok_or_else(overflow)?;
    schedule.total = vested;
    schedule.revoked_at = Some(clock.now);
    clock.keep();
    save(&schedule);
    if refund > 0 {
        sdk::asset_transfer(&schedule.asset, &schedule.creator, refund);
    }
    sdk::emit(&("revoke", id, refund));
    Ok(refund)
}

/// `schedule(id) -> Option<Schedule>`: `None` for an ID no schedule has.
pub fn schedule(id: u64) -> Result<Option<Schedule>, Vec<u8>> {
    count()?;
    storage::read(&schedule_key(id))
}

/// `releasable(id)`: what `release(id)` would pay now.
pub fn releasable(id: u64) -> Result<u128, Vec<u8>> {
    let now = Clock::read()?.now;
    load(id)?.releasable(now).ok_or_else(overflow)
}

/// `vested(id, at)`: what has vested by the time `at`; for a revoked schedule its reduced total, whatever `at` is.
pub fn vested(id: u64, at: u64) -> Result<u128, Vec<u8>> {
    load(id)?.vested_at(at).ok_or_else(overflow)
}

/// `count()`: how many schedules have been created; their IDs run from 1 to it.
pub fn count() -> Result<u64, Vec<u8>> {
    storage::read(KEY_COUNT)?.ok_or_else(|| err(ERR_NOT_INITIALIZED))
}

/// `by_beneficiary(account, start, limit)`: the schedules paying `account`, in creation order, from position `start`
/// on, at most `limit` of them and at most [`MAX_PAGE`].
pub fn by_beneficiary(
    account: &[u8; 32],
    start: u64,
    limit: u32,
) -> Result<Vec<Schedule>, Vec<u8>> {
    BENEFICIARY_LIST.page(account, start, limit)
}

/// `by_creator(account, start, limit)`: the schedules `account` created, in creation order, from position `start`
/// on, at most `limit` of them and at most [`MAX_PAGE`].
pub fn by_creator(account: &[u8; 32], start: u64, limit: u32) -> Result<Vec<Schedule>, Vec<u8>> {
    CREATOR_LIST.page(account, start, limit)
}
