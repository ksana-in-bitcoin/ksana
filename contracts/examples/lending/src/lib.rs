//! Example contract: an isolated lending market for two VM built-in assets (SPEC §9.5, D-073).
//!
//! Suppliers deposit the **loan asset** and earn interest; borrowers deposit the **collateral asset** and borrow the
//! loan asset against it. Both are built-in assets (KSA or assets that contracts issue with `asset_issue`): they come in
//! attached to calls and leave with `asset_transfer`. The interest rate follows the market's utilization (a kinked
//! model), and anyone can liquidate an unhealthy position at a bonus.
//!
//! **The admin sets the price.** Ksana has no price oracle, so the price of the collateral in the loan asset is whatever
//! the market's admin (the deployer) last set with `set_price`: the admin can make any position liquidatable at any
//! moment, or let borrowers take out more than their collateral is worth. Use a market only with an admin you trust, or
//! one whose admin was set to `None` (the price is then frozen for good). This is an example to learn from, not a
//! production protocol: it has not been audited and leaves out much of what a real one needs (an oracle, caps, pausing).
//!
//! Units:
//!
//! - `WAD` = 10^18 fixed point ([`math::WAD`]); the borrow index starts at `WAD`.
//! - `price`: loan-asset base units per one collateral base unit, times `WAD` (base units on both sides, so the decimals
//!   are already in it). Both assets with 8 decimals and 1 collateral token worth 2 loan tokens: `price = 2 · WAD`.
//! - Rates are basis points a year (10 000 = 100%), a year being [`SECONDS_PER_YEAR`] = 31 536 000 seconds; ratios
//!   (`*_bps`) are basis points of a value.
//!
//! Arguments and return values are Borsh-encoded. The arguments are a tuple in the order of the table below (a single
//! argument is just itself; no arguments is empty bytes):
//!
//! | Method | Arguments | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `(loan_asset: [u8; 32], collateral_asset: [u8; 32], price: u128, params: Params)` | `()` |
//! | `supply` | none, the loan asset attached | `u128` (shares received) |
//! | `withdraw` | `shares: u128` | `u128` (loan asset paid out) |
//! | `deposit_collateral` | none, the collateral asset attached | `u128` (the caller's collateral) |
//! | `withdraw_collateral` | `amount: u128` | `u128` (the caller's remaining collateral) |
//! | `borrow` | `amount: u128` | `u128` (the caller's debt) |
//! | `repay` | `borrower: [u8; 32]`, the loan asset attached | `u128` (the borrower's remaining debt) |
//! | `liquidate` | `borrower: [u8; 32]`, the loan asset attached | `(u128, u128)` (repaid, collateral seized) |
//! | `set_price` | `price: u128` | `()` |
//! | `set_admin` | `admin: Option<[u8; 32]>` | `()` |
//! | `withdraw_reserves` | `(amount: u128, to: [u8; 32])` | `()` |
//! | `market` | none | [`Market`] |
//! | `position` | `account: [u8; 32]` | [`Position`] |
//!
//! Rules:
//!
//! - `init`: the deployer becomes the admin. The two assets differ, `price > 0`,
//!   `0 < collateral_factor_bps ≤ liquidation_threshold_bps < 10 000`, `liquidation_bonus_bps ≤ 2 000` and
//!   `liquidation_threshold_bps · (10 000 + liquidation_bonus_bps) ≤ 10 000 · 10 000` (a liquidation at the threshold
//!   never needs more collateral than there is), `reserve_factor_bps ≤ 5 000`, `0 < kink_bps ≤ 10 000`, and
//!   `base_rate_bps`, `slope1_bps` and `slope2_bps` at most 100 000 each (1 000% a year). The assets are not checked: a
//!   built-in asset exists once it has been issued, and KSA always does.
//! - Interest: every method that changes state first accrues interest up to the block's timestamp, and the views compute
//!   the same without writing. With `dt` the seconds since the last accrual (0 when the block's timestamp is not after
//!   it: producers set timestamps only roughly, and one that goes back accrues nothing), the utilization
//!   `U = total_borrows · 10 000 / (cash + total_borrows − reserves)` (0 when that is 0, at most 10 000) sets the borrow
//!   rate `base + slope1 · min(U, kink) / kink + slope2 · (U − kink) / (10 000 − kink)` (the last term only above the
//!   kink). With `rate_per_second = rate · WAD / 10 000 / 31 536 000`, the borrow index grows by
//!   `index · rate_per_second · dt / WAD`, `total_borrows` grows by the same factor (`total_borrows · new_index /
//!   index`, rounded down, so it never runs ahead of the debts), and `reserve_factor_bps` of that interest is added to
//!   `reserves`, the admin's claim on `cash`. The supply rate `market` shows is
//!   `borrow_rate · U · (10 000 − reserve_factor) / 10 000 / 10 000`.
//! - Supplying: suppliers hold shares of `total_assets = cash + total_borrows − reserves` (0 when negative). `supply`
//!   takes the attached loan asset for `amount · total_shares / total_assets` shares (the amount itself for the first
//!   shares), rounded down and above 0. `withdraw` pays `shares · total_assets / total_shares`, rounded down, out of the
//!   cash neither lent nor reserved, `cash − reserves`.
//! - Borrowing: `collateral_value = collateral · price / WAD` (rounded down) in loan-asset units; the borrow limit is
//!   `collateral_value · collateral_factor_bps / 10 000` and the liquidation limit
//!   `collateral_value · liquidation_threshold_bps / 10 000`. `borrow` and `withdraw_collateral` keep the caller's debt
//!   within the borrow limit, and `borrow` takes at most `cash − reserves`. A debt is stored as `(principal, index)`
//!   and is `principal · borrow_index / index` now, rounded up, so the market is never short. `repay` takes the
//!   attached loan asset for any borrower and refunds what exceeds the debt. `total_borrows` trails the sum of the
//!   debts by their rounding (less than a unit for each accrual and for each change of a debt), so it decreases with
//!   saturation and never fails a repayment.
//! - Liquidation: anyone but the borrower can repay a debt above the liquidation limit, at most half of it (rounded
//!   up), for `repaid · (10 000 + liquidation_bonus_bps) / 10 000 · WAD / price` of the borrower's collateral (rounded
//!   down at each step, at most all of it); the rest of the attached amount is refunded. The liquidator pays `repaid`
//!   even when the seizure is capped by the collateral, so attach no more than the collateral is worth. When the
//!   collateral is gone and debt remains, the rest of the debt is written off: the suppliers bear the loss.
//! - The admin sets the price, hands the role on (`None` leaves the market without an admin: the price can never change
//!   again), and withdraws reserves, at most `reserves` and at most `cash`.
//! - Attached funds: `supply`, `repay` and `liquidate` take exactly one item, the loan asset; `deposit_collateral`
//!   exactly one, the collateral asset; every other method, `init` and the views included, refuses attached funds.
//! - Every product goes through [`math::mul_div`] (exact 256-bit intermediate) or checked arithmetic, and a result that
//!   does not fit in u128 reverts with `arithmetic overflow`, with three exceptions where a revert would only block a
//!   method that must keep working: a collateral value above `u128::MAX` counts as `u128::MAX`, `health_bps` saturates
//!   at `u128::MAX`, and a seizure above `u128::MAX` is capped at the collateral like any other. The borrow index has
//!   room to grow by about e^47: decades at realistic rates, but rates near their upper bounds at full utilization
//!   exhaust it within a few years, after which every method reverts. Choose realistic rates.
//!
//! Reads, then checks, then writes, then transfers (checks-effects-interactions); a failed transfer fails the whole
//! operation. Storage: `config` (assets and [`Params`]), `admin` (absent once there is none), `price`, `state` (the
//! market's totals, borrow index and last accrual), and per account `s ‖ account` (shares), `c ‖ account` (collateral)
//! and `d ‖ account` (the debt's `(principal, index)`), each removed when zero.
//!
//! Events (Borsh, the name first): `("supply", account, amount, shares)`, `("withdraw", account, shares, amount)`,
//! `("deposit_collateral", account, amount)`, `("withdraw_collateral", account, amount)`, `("borrow", account, amount)`,
//! `("repay", payer, borrower, amount)`, `("liquidate", liquidator, borrower, repaid, seized)`,
//! `("write_off", borrower, amount)`, `("price", price)`, `("admin", previous, admin)` and
//! `("withdraw_reserves", to, amount)`. `init` emits `("admin", None, Some(deployer))` and `("price", price)`.
//!
//! ```abi
//! struct Params { collateral_factor_bps: u32, liquidation_threshold_bps: u32, liquidation_bonus_bps: u32, reserve_factor_bps: u32, base_rate_bps: u32, slope1_bps: u32, kink_bps: u32, slope2_bps: u32 }
//! struct Market { loan_asset: AssetId, collateral_asset: AssetId, admin: Option<Account>, price: u128, params: Params, cash: u128, total_borrows: u128, reserves: u128, total_shares: u128, total_collateral: u128, borrow_index: u128, last_accrual: u64, utilization_bps: u32, borrow_rate_bps: u32, supply_rate_bps: u32 }
//! struct Position { shares: u128, supplied: u128, collateral: u128, debt: u128, borrow_limit: u128, liquidation_limit: u128, health_bps: Option<u128> }
//! init(loan_asset: AssetId, collateral_asset: AssetId, price: u128, params: Params)   // the deployer becomes the admin; price is loan units per collateral unit × 10^18
//! payable call supply() -> u128                          // attach the loan asset; returns the shares received
//! call withdraw(shares: u128) -> u128                    // returns the loan asset paid out
//! payable call deposit_collateral() -> u128              // attach the collateral asset; returns the caller's collateral
//! call withdraw_collateral(amount: u128) -> u128         // returns the caller's remaining collateral
//! call borrow(amount: u128) -> u128                      // returns the caller's debt
//! payable call repay(borrower: Account) -> u128          // attach the loan asset; the excess is refunded; returns the remaining debt
//! payable call liquidate(borrower: Account) -> (u128, u128)   // attach the loan asset; returns (repaid, collateral seized)
//! call set_price(price: u128)                            // admin only; the admin can make any position liquidatable
//! call set_admin(admin: Option<Account>)                 // admin only; None freezes the price for good
//! call withdraw_reserves(amount: u128, to: Account)      // admin only
//! view market() -> Market                                // with interest accrued to the current block's time
//! view position(account: Account) -> Position            // likewise; health_bps below 10000 means liquidatable
//! event supply(account: Account, amount: u128, shares: u128)
//! event withdraw(account: Account, shares: u128, amount: u128)
//! event deposit_collateral(account: Account, amount: u128)
//! event withdraw_collateral(account: Account, amount: u128)
//! event borrow(account: Account, amount: u128)
//! event repay(payer: Account, borrower: Account, amount: u128)
//! event liquidate(liquidator: Account, borrower: Account, repaid: u128, seized: u128)
//! event write_off(borrower: Account, amount: u128)
//! event price(price: u128)
//! event admin(previous: Option<Account>, admin: Option<Account>)
//! event withdraw_reserves(to: Account, amount: u128)
//! ```

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::vec::Vec;

use ksana_contract_sdk::borsh::BorshSerialize;
use ksana_contract_sdk::{self as sdk, storage, CallResult, Decode};

use math::{mul_div, mul_div_up, WAD};

sdk::entry!(dispatch);

/// One year in seconds, the unit of the interest rates.
pub const SECONDS_PER_YEAR: u64 = 31_536_000;
/// 100% in basis points.
pub const BPS: u32 = 10_000;
/// Upper bound of `base_rate_bps`, `slope1_bps` and `slope2_bps`, each: 1 000% a year.
pub const MAX_RATE_BPS: u32 = 100_000;
/// Upper bound of `liquidation_bonus_bps`.
pub const MAX_LIQUIDATION_BONUS_BPS: u32 = 2_000;
/// Upper bound of `reserve_factor_bps`.
pub const MAX_RESERVE_FACTOR_BPS: u32 = 5_000;
/// `10 000 · 10 000`: the bound of `liquidation_threshold_bps · (10 000 + liquidation_bonus_bps)`.
const MAX_THRESHOLD_WITH_BONUS: u64 = 100_000_000;

const KEY_CONFIG: &[u8] = b"config";
const KEY_ADMIN: &[u8] = b"admin";
const KEY_PRICE: &[u8] = b"price";
const KEY_STATE: &[u8] = b"state";
const PREFIX_SHARES: &[u8] = b"s";
const PREFIX_COLLATERAL: &[u8] = b"c";
const PREFIX_DEBT: &[u8] = b"d";

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_SAME_ASSET: &[u8] = b"loan and collateral assets must differ";
const ERR_ZERO_PRICE: &[u8] = b"price must be positive";
const ERR_BAD_FACTORS: &[u8] = b"invalid collateral factor or liquidation threshold";
const ERR_BAD_BONUS: &[u8] = b"invalid liquidation bonus";
const ERR_BAD_RESERVE_FACTOR: &[u8] = b"invalid reserve factor";
const ERR_BAD_RATES: &[u8] = b"invalid interest rate model";
const ERR_UNEXPECTED_FUNDS: &[u8] = b"unexpected attached funds";
const ERR_ATTACH_LOAN: &[u8] = b"attach only the loan asset";
const ERR_ATTACH_COLLATERAL: &[u8] = b"attach only the collateral asset";
const ERR_ZERO_AMOUNT: &[u8] = b"amount must be positive";
const ERR_TOO_SMALL: &[u8] = b"amount too small";
const ERR_INSOLVENT: &[u8] = b"market is insolvent";
const ERR_INSUFFICIENT_SHARES: &[u8] = b"insufficient shares";
const ERR_INSUFFICIENT_LIQUIDITY: &[u8] = b"insufficient liquidity";
const ERR_INSUFFICIENT_COLLATERAL: &[u8] = b"insufficient collateral";
const ERR_UNHEALTHY: &[u8] = b"position would be unhealthy";
const ERR_BORROW_LIMIT: &[u8] = b"borrow limit exceeded";
const ERR_NO_DEBT: &[u8] = b"no debt";
const ERR_SELF_LIQUIDATION: &[u8] = b"cannot liquidate yourself";
const ERR_HEALTHY: &[u8] = b"position is healthy";
const ERR_NOT_ADMIN: &[u8] = b"only the admin";
const ERR_ZERO_ACCOUNT: &[u8] = b"zero account";
const ERR_EXCEEDS_RESERVES: &[u8] = b"amount exceeds reserves";
const ERR_INSUFFICIENT_CASH: &[u8] = b"insufficient cash";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";

/// The parameters of a market, fixed at `init`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Params {
    /// The share of the collateral's value that can be borrowed, in basis points.
    pub collateral_factor_bps: u32,
    /// The share of the collateral's value a debt may reach before it can be liquidated, in basis points.
    pub liquidation_threshold_bps: u32,
    /// The bonus a liquidator receives in collateral, in basis points of the debt repaid.
    pub liquidation_bonus_bps: u32,
    /// The share of the interest kept as reserves for the admin, in basis points.
    pub reserve_factor_bps: u32,
    /// The borrow rate at zero utilization, in basis points a year.
    pub base_rate_bps: u32,
    /// What the borrow rate adds from zero utilization up to the kink, in basis points a year.
    pub slope1_bps: u32,
    /// The utilization where the second slope starts, in basis points.
    pub kink_bps: u32,
    /// What the borrow rate adds from the kink up to full utilization, in basis points a year.
    pub slope2_bps: u32,
}

impl Decode for Params {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            collateral_factor_bps: u32::decode(input)?,
            liquidation_threshold_bps: u32::decode(input)?,
            liquidation_bonus_bps: u32::decode(input)?,
            reserve_factor_bps: u32::decode(input)?,
            base_rate_bps: u32::decode(input)?,
            slope1_bps: u32::decode(input)?,
            kink_bps: u32::decode(input)?,
            slope2_bps: u32::decode(input)?,
        })
    }
}

/// The market as `market` returns it, with interest accrued to the current block's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Market {
    /// The asset suppliers lend and borrowers borrow.
    pub loan_asset: [u8; 32],
    /// The asset borrowers pledge.
    pub collateral_asset: [u8; 32],
    /// The account that sets the price; `None` once the role has been given up.
    pub admin: Option<[u8; 32]>,
    /// Loan-asset base units per collateral base unit, times `WAD`.
    pub price: u128,
    /// The parameters fixed at `init`.
    pub params: Params,
    /// The loan asset the contract holds for the market, reserves included.
    pub cash: u128,
    /// What the borrowers owe, interest included.
    pub total_borrows: u128,
    /// The admin's part of the interest, a claim on `cash`.
    pub reserves: u128,
    /// All suppliers' shares.
    pub total_shares: u128,
    /// The collateral asset the contract holds for the borrowers.
    pub total_collateral: u128,
    /// The borrow index (`WAD` at `init`).
    pub borrow_index: u128,
    /// The block timestamp interest was last accrued to.
    pub last_accrual: u64,
    /// `total_borrows` over the suppliers' assets, in basis points.
    pub utilization_bps: u32,
    /// The borrow rate at this utilization, in basis points a year.
    pub borrow_rate_bps: u32,
    /// The rate the suppliers earn at this utilization, in basis points a year.
    pub supply_rate_bps: u32,
}

impl Decode for Market {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            loan_asset: <[u8; 32]>::decode(input)?,
            collateral_asset: <[u8; 32]>::decode(input)?,
            admin: Option::<[u8; 32]>::decode(input)?,
            price: u128::decode(input)?,
            params: Params::decode(input)?,
            cash: u128::decode(input)?,
            total_borrows: u128::decode(input)?,
            reserves: u128::decode(input)?,
            total_shares: u128::decode(input)?,
            total_collateral: u128::decode(input)?,
            borrow_index: u128::decode(input)?,
            last_accrual: u64::decode(input)?,
            utilization_bps: u32::decode(input)?,
            borrow_rate_bps: u32::decode(input)?,
            supply_rate_bps: u32::decode(input)?,
        })
    }
}

/// An account's position as `position` returns it, with interest accrued to the current block's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Position {
    /// The account's supply shares.
    pub shares: u128,
    /// What the shares are worth in the loan asset, rounded down.
    pub supplied: u128,
    /// The collateral the account has deposited.
    pub collateral: u128,
    /// The account's debt, rounded up.
    pub debt: u128,
    /// The most the account may owe after a `borrow` or `withdraw_collateral`.
    pub borrow_limit: u128,
    /// The debt above which the account can be liquidated.
    pub liquidation_limit: u128,
    /// `liquidation_limit · 10 000 / debt` (`None` without debt); below 10 000 the account can be liquidated.
    pub health_bps: Option<u128>,
}

impl Decode for Position {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            shares: u128::decode(input)?,
            supplied: u128::decode(input)?,
            collateral: u128::decode(input)?,
            debt: u128::decode(input)?,
            borrow_limit: u128::decode(input)?,
            liquidation_limit: u128::decode(input)?,
            health_bps: Option::<u128>::decode(input)?,
        })
    }
}

/// The assets and parameters, written once by `init`.
#[derive(Clone, Copy, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
struct Config {
    loan_asset: [u8; 32],
    collateral_asset: [u8; 32],
    params: Params,
}

impl Decode for Config {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            loan_asset: <[u8; 32]>::decode(input)?,
            collateral_asset: <[u8; 32]>::decode(input)?,
            params: Params::decode(input)?,
        })
    }
}

/// The market's books, under one storage key.
#[derive(Clone, Copy, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
struct State {
    cash: u128,
    total_borrows: u128,
    reserves: u128,
    total_shares: u128,
    total_collateral: u128,
    borrow_index: u128,
    last_accrual: u64,
}

impl Decode for State {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            cash: u128::decode(input)?,
            total_borrows: u128::decode(input)?,
            reserves: u128::decode(input)?,
            total_shares: u128::decode(input)?,
            total_collateral: u128::decode(input)?,
            borrow_index: u128::decode(input)?,
            last_accrual: u64::decode(input)?,
        })
    }
}

/// Exact products of two u128 values divided by a third (no host access).
pub mod math {
    /// 10^18: the fixed point of prices and of the borrow index.
    pub const WAD: u128 = 1_000_000_000_000_000_000;

    /// The low 64 bits of a u128.
    const LOW: u128 = 0xffff_ffff_ffff_ffff;

    /// `floor(a · b / c)` with the product kept exactly in 256 bits; `None` when `c` is 0 or the quotient exceeds
    /// `u128::MAX`.
    pub fn mul_div(a: u128, b: u128, c: u128) -> Option<u128> {
        let (quotient, _) = div_wide(mul_wide(a, b)?, c)?;
        Some(quotient)
    }

    /// `ceil(a · b / c)` with the product kept exactly in 256 bits; `None` when `c` is 0 or the quotient exceeds
    /// `u128::MAX`.
    pub fn mul_div_up(a: u128, b: u128, c: u128) -> Option<u128> {
        let (quotient, remainder) = div_wide(mul_wide(a, b)?, c)?;
        if remainder == 0 {
            Some(quotient)
        } else {
            quotient.checked_add(1)
        }
    }

    /// The product `a · b` as its high and low 128-bit halves, from the four products of the 64-bit halves. None of
    /// the checked operations can fail: each partial product is at most `(2^64 − 1)^2`, the middle column is below
    /// `3 · 2^64`, and the high half of a product of two u128 values is below `2^128`.
    fn mul_wide(a: u128, b: u128) -> Option<(u128, u128)> {
        let (a_high, a_low) = (a >> 64, a & LOW);
        let (b_high, b_low) = (b >> 64, b & LOW);
        let low_low = a_low.checked_mul(b_low)?;
        let low_high = a_low.checked_mul(b_high)?;
        let high_low = a_high.checked_mul(b_low)?;
        let high_high = a_high.checked_mul(b_high)?;
        let middle = (low_low >> 64)
            .checked_add(low_high & LOW)?
            .checked_add(high_low & LOW)?;
        let low = (middle << 64) | (low_low & LOW);
        let high = high_high
            .checked_add(low_high >> 64)?
            .checked_add(high_low >> 64)?
            .checked_add(middle >> 64)?;
        Some((high, low))
    }

    /// `(high · 2^128 + low) / divisor` as `(quotient, remainder)`; `None` when the divisor is 0 or the quotient does
    /// not fit in u128, which is exactly when `high ≥ divisor`.
    fn div_wide((high, low): (u128, u128), divisor: u128) -> Option<(u128, u128)> {
        if divisor == 0 || high >= divisor {
            return None;
        }
        if high == 0 {
            return Some((low.checked_div(divisor)?, low.checked_rem(divisor)?));
        }
        // Binary long division, bringing down the bits of `low` one by one with the remainder starting at `high`.
        // The remainder stays below the divisor, so doubling it and adding a bit gives a value below twice the
        // divisor. That value may carry out of u128; it then exceeds the divisor, and subtracting the divisor once,
        // wrapping, leaves the exact remainder (which fits again).
        let mut remainder = high;
        let mut quotient = 0u128;
        for bit in (0..128u32).rev() {
            let carry = remainder >> 127;
            remainder = (remainder << 1) | ((low >> bit) & 1);
            quotient <<= 1;
            if carry != 0 || remainder >= divisor {
                remainder = remainder.wrapping_sub(divisor);
                quotient |= 1;
            }
        }
        Some((quotient, remainder))
    }
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let (loan_asset, collateral_asset, price, params) = sdk::decode(args)?;
            init(loan_asset, collateral_asset, price, params)?;
            sdk::ok(&())
        }
        "supply" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&supply()?)
        }
        "withdraw" => {
            let shares = sdk::decode(args)?;
            sdk::ok(&withdraw(shares)?)
        }
        "deposit_collateral" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&deposit_collateral()?)
        }
        "withdraw_collateral" => {
            let amount = sdk::decode(args)?;
            sdk::ok(&withdraw_collateral(amount)?)
        }
        "borrow" => {
            let amount = sdk::decode(args)?;
            sdk::ok(&borrow(amount)?)
        }
        "repay" => {
            let borrower = sdk::decode(args)?;
            sdk::ok(&repay(&borrower)?)
        }
        "liquidate" => {
            let borrower = sdk::decode(args)?;
            sdk::ok(&liquidate(&borrower)?)
        }
        "set_price" => {
            let price = sdk::decode(args)?;
            set_price(price)?;
            sdk::ok(&())
        }
        "set_admin" => {
            let admin = sdk::decode(args)?;
            set_admin(admin)?;
            sdk::ok(&())
        }
        "withdraw_reserves" => {
            let (amount, to) = sdk::decode(args)?;
            withdraw_reserves(amount, &to)?;
            sdk::ok(&())
        }
        "market" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&market()?)
        }
        "position" => {
            let account = sdk::decode(args)?;
            sdk::ok(&position(&account)?)
        }
        _ => sdk::revert("unknown method"),
    }
}

fn overflow() -> Vec<u8> {
    ERR_OVERFLOW.to_vec()
}

fn account_key(prefix: &[u8], account: &[u8; 32]) -> Vec<u8> {
    [prefix, account.as_slice()].concat()
}

/// Reads a u128 amount; 0 when absent.
fn read_amount(key: &[u8]) -> Result<u128, Vec<u8>> {
    Ok(storage::read::<u128>(key)?.unwrap_or(0))
}

/// Writes a u128 amount; 0 removes the key, so that each state has exactly one representation.
fn write_amount(key: &[u8], amount: u128) {
    if amount == 0 {
        storage::remove(key);
    } else {
        storage::write(key, &amount);
    }
}

fn config() -> Result<Config, Vec<u8>> {
    storage::read(KEY_CONFIG)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

fn admin() -> Result<Option<[u8; 32]>, Vec<u8>> {
    storage::read(KEY_ADMIN)
}

fn price() -> Result<u128, Vec<u8>> {
    storage::read(KEY_PRICE)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

/// The stored state with interest accrued to the current block's timestamp (not written).
fn accrued_state(params: &Params) -> Result<State, Vec<u8>> {
    let mut state: State = storage::read(KEY_STATE)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())?;
    accrue(&mut state, params, sdk::block_timestamp())?;
    Ok(state)
}

/// Refuses attached funds (methods that are not payable).
fn no_funds() -> Result<(), Vec<u8>> {
    if sdk::call_funds().is_empty() {
        Ok(())
    } else {
        Err(ERR_UNEXPECTED_FUNDS.to_vec())
    }
}

/// The amount of `asset` attached as the call's only item; anything else reverts with `reason`.
fn attached_only(asset: &[u8; 32], reason: &[u8]) -> Result<u128, Vec<u8>> {
    match sdk::call_funds().as_slice() {
        [funds] if funds.asset == *asset && funds.amount > 0 => Ok(funds.amount),
        _ => Err(reason.to_vec()),
    }
}

/// The caller, when it is the admin.
fn only_admin() -> Result<[u8; 32], Vec<u8>> {
    let caller = sdk::caller();
    match admin()? {
        Some(admin) if admin == caller => Ok(caller),
        _ => Err(ERR_NOT_ADMIN.to_vec()),
    }
}

/// The debt of `account` at borrow index `index`: `principal · index / snapshot`, rounded up.
fn debt_of(account: &[u8; 32], index: u128) -> Result<u128, Vec<u8>> {
    match storage::read::<(u128, u128)>(&account_key(PREFIX_DEBT, account))? {
        None => Ok(0),
        Some((principal, snapshot)) => mul_div_up(principal, index, snapshot).ok_or_else(overflow),
    }
}

/// Stores the debt of `account` as `(debt, index)`; 0 removes the key.
fn write_debt(account: &[u8; 32], debt: u128, index: u128) {
    let key = account_key(PREFIX_DEBT, account);
    if debt == 0 {
        storage::remove(&key);
    } else {
        storage::write(&key, &(debt, index));
    }
}

/// `value · bps / 10 000`, rounded down.
fn bps_of(value: u128, bps: u32) -> Result<u128, Vec<u8>> {
    mul_div(value, u128::from(bps), u128::from(BPS)).ok_or_else(overflow)
}

/// The borrow limit and the liquidation limit of `collateral` at `price`. The collateral's value is
/// `collateral · price / WAD`, rounded down, and counts as `u128::MAX` above it.
fn limits(collateral: u128, price: u128, params: &Params) -> Result<(u128, u128), Vec<u8>> {
    let value = mul_div(collateral, price, WAD).unwrap_or(u128::MAX);
    Ok((
        bps_of(value, params.collateral_factor_bps)?,
        bps_of(value, params.liquidation_threshold_bps)?,
    ))
}

impl State {
    /// The suppliers' assets: `cash + total_borrows − reserves`, 0 when that is negative.
    fn total_assets(&self) -> Result<u128, Vec<u8>> {
        if self.cash >= self.reserves {
            self.liquidity()
                .checked_add(self.total_borrows)
                .ok_or_else(overflow)
        } else {
            let short = self.reserves.saturating_sub(self.cash);
            Ok(self.total_borrows.saturating_sub(short))
        }
    }

    /// The cash neither lent nor reserved: what withdrawals and borrows can take.
    fn liquidity(&self) -> u128 {
        self.cash.saturating_sub(self.reserves)
    }

    /// `total_borrows · 10 000 / total_assets`: 0 when the assets are 0, at most 10 000.
    fn utilization_bps(&self) -> Result<u32, Vec<u8>> {
        let assets = self.total_assets()?;
        if assets == 0 {
            return Ok(0);
        }
        // Above u128::MAX the utilization is certainly above 10 000 (the assets are not 0).
        let utilization = mul_div(self.total_borrows, u128::from(BPS), assets)
            .unwrap_or(u128::MAX)
            .min(u128::from(BPS));
        u32::try_from(utilization).map_err(|_| overflow())
    }
}

/// The borrow rate at `utilization`, in basis points a year:
/// `base + slope1 · min(U, kink) / kink + slope2 · (U − kink) / (10 000 − kink)`, the last term only above the kink.
fn borrow_rate_bps(params: &Params, utilization: u32) -> Result<u32, Vec<u8>> {
    let kink = params.kink_bps;
    let first = mul_div(
        u128::from(params.slope1_bps),
        u128::from(utilization.min(kink)),
        u128::from(kink),
    )
    .ok_or_else(overflow)?;
    let second = match utilization.checked_sub(kink) {
        Some(above) if above > 0 => {
            let span = BPS.checked_sub(kink).ok_or_else(overflow)?;
            mul_div(
                u128::from(params.slope2_bps),
                u128::from(above),
                u128::from(span),
            )
            .ok_or_else(overflow)?
        }
        _ => 0,
    };
    let rate = u128::from(params.base_rate_bps)
        .checked_add(first)
        .and_then(|rate| rate.checked_add(second))
        .ok_or_else(overflow)?;
    u32::try_from(rate).map_err(|_| overflow())
}

/// The supply rate: `borrow_rate · U · (10 000 − reserve_factor) / 10 000 / 10 000`, in basis points a year.
fn supply_rate_bps(
    borrow_rate: u32,
    utilization: u32,
    reserve_factor: u32,
) -> Result<u32, Vec<u8>> {
    let kept = BPS.checked_sub(reserve_factor).ok_or_else(overflow)?;
    let rate = u128::from(borrow_rate)
        .checked_mul(u128::from(utilization))
        .and_then(|rate| rate.checked_mul(u128::from(kept)))
        .and_then(|rate| rate.checked_div(u128::from(BPS)))
        .and_then(|rate| rate.checked_div(u128::from(BPS)))
        .ok_or_else(overflow)?;
    u32::try_from(rate).map_err(|_| overflow())
}

/// Accrues interest from `last_accrual` to `now`; nothing when `now` is not after `last_accrual`.
///
/// The interest on `total_borrows` is taken from the index's own growth, `total_borrows · new_index / index` rounded
/// down, rather than from `rate · dt` again: each debt grows by that same ratio and is rounded up, so the sum of the
/// debts never falls below `total_borrows`, however large the amounts.
fn accrue(state: &mut State, params: &Params, now: u64) -> Result<(), Vec<u8>> {
    let dt = match now.checked_sub(state.last_accrual) {
        Some(dt) if dt > 0 => dt,
        _ => return Ok(()),
    };
    let rate = borrow_rate_bps(params, state.utilization_bps()?)?;
    let rate_per_second = u128::from(rate)
        .checked_mul(WAD)
        .and_then(|rate| rate.checked_div(u128::from(BPS)))
        .and_then(|rate| rate.checked_div(u128::from(SECONDS_PER_YEAR)))
        .ok_or_else(overflow)?;
    let growth = rate_per_second
        .checked_mul(u128::from(dt))
        .ok_or_else(overflow)?;
    let index = mul_div(state.borrow_index, growth, WAD)
        .and_then(|increase| state.borrow_index.checked_add(increase))
        .ok_or_else(overflow)?;
    let total_borrows =
        mul_div(state.total_borrows, index, state.borrow_index).ok_or_else(overflow)?;
    let interest = total_borrows
        .checked_sub(state.total_borrows)
        .ok_or_else(overflow)?;
    let reserves = bps_of(interest, params.reserve_factor_bps)?
        .checked_add(state.reserves)
        .ok_or_else(overflow)?;
    state.borrow_index = index;
    state.total_borrows = total_borrows;
    state.reserves = reserves;
    state.last_accrual = now;
    Ok(())
}

fn check_params(params: &Params) -> Result<(), Vec<u8>> {
    let factor = params.collateral_factor_bps;
    let threshold = params.liquidation_threshold_bps;
    if factor == 0 || factor > threshold || threshold >= BPS {
        return Err(ERR_BAD_FACTORS.to_vec());
    }
    let bonus = params.liquidation_bonus_bps;
    if bonus > MAX_LIQUIDATION_BONUS_BPS {
        return Err(ERR_BAD_BONUS.to_vec());
    }
    // A liquidation at the threshold seizes `threshold · (1 + bonus)` of the collateral's value: at most all of it
    let threshold_with_bonus = BPS
        .checked_add(bonus)
        .and_then(|with_bonus| u64::from(threshold).checked_mul(u64::from(with_bonus)))
        .ok_or_else(overflow)?;
    if threshold_with_bonus > MAX_THRESHOLD_WITH_BONUS {
        return Err(ERR_BAD_BONUS.to_vec());
    }
    if params.reserve_factor_bps > MAX_RESERVE_FACTOR_BPS {
        return Err(ERR_BAD_RESERVE_FACTOR.to_vec());
    }
    if params.kink_bps == 0
        || params.kink_bps > BPS
        || params.base_rate_bps > MAX_RATE_BPS
        || params.slope1_bps > MAX_RATE_BPS
        || params.slope2_bps > MAX_RATE_BPS
    {
        return Err(ERR_BAD_RATES.to_vec());
    }
    Ok(())
}

/// `init(loan_asset, collateral_asset, price, params)`: opens the market, with the deployer as its admin.
pub fn init(
    loan_asset: [u8; 32],
    collateral_asset: [u8; 32],
    price: u128,
    params: Params,
) -> Result<(), Vec<u8>> {
    if storage::get(KEY_CONFIG).is_some() {
        return Err(ERR_ALREADY_INITIALIZED.to_vec());
    }
    // Funds attached at deployment would have nowhere to be accounted for
    no_funds()?;
    let admin = sdk::caller();
    if admin == [0u8; 32] {
        return Err(ERR_ZERO_ACCOUNT.to_vec());
    }
    if loan_asset == collateral_asset {
        return Err(ERR_SAME_ASSET.to_vec());
    }
    if price == 0 {
        return Err(ERR_ZERO_PRICE.to_vec());
    }
    check_params(&params)?;
    let state = State {
        cash: 0,
        total_borrows: 0,
        reserves: 0,
        total_shares: 0,
        total_collateral: 0,
        borrow_index: WAD,
        last_accrual: sdk::block_timestamp(),
    };
    storage::write(
        KEY_CONFIG,
        &Config {
            loan_asset,
            collateral_asset,
            params,
        },
    );
    storage::write(KEY_ADMIN, &admin);
    storage::write(KEY_PRICE, &price);
    storage::write(KEY_STATE, &state);
    sdk::emit(&("admin", None::<[u8; 32]>, Some(admin)));
    sdk::emit(&("price", price));
    Ok(())
}

/// `supply() -> shares`: takes the attached loan asset and credits the caller with shares of the suppliers' assets.
pub fn supply() -> Result<u128, Vec<u8>> {
    let config = config()?;
    let amount = attached_only(&config.loan_asset, ERR_ATTACH_LOAN)?;
    let mut state = accrued_state(&config.params)?;
    let shares = if state.total_shares == 0 {
        amount
    } else {
        let assets = state.total_assets()?;
        if assets == 0 {
            return Err(ERR_INSOLVENT.to_vec());
        }
        mul_div(amount, state.total_shares, assets).ok_or_else(overflow)?
    };
    if shares == 0 {
        return Err(ERR_TOO_SMALL.to_vec());
    }
    let account = sdk::caller();
    let key = account_key(PREFIX_SHARES, &account);
    let held = read_amount(&key)?
        .checked_add(shares)
        .ok_or_else(overflow)?;
    state.cash = state.cash.checked_add(amount).ok_or_else(overflow)?;
    state.total_shares = state
        .total_shares
        .checked_add(shares)
        .ok_or_else(overflow)?;
    storage::write(KEY_STATE, &state);
    write_amount(&key, held);
    sdk::emit(&("supply", account, amount, shares));
    Ok(shares)
}

/// `withdraw(shares) -> amount`: redeems the caller's shares for the loan asset.
pub fn withdraw(shares: u128) -> Result<u128, Vec<u8>> {
    let config = config()?;
    no_funds()?;
    if shares == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    let account = sdk::caller();
    let key = account_key(PREFIX_SHARES, &account);
    let remaining = read_amount(&key)?
        .checked_sub(shares)
        .ok_or_else(|| ERR_INSUFFICIENT_SHARES.to_vec())?;
    let mut state = accrued_state(&config.params)?;
    let assets = state.total_assets()?;
    if assets == 0 {
        return Err(ERR_INSOLVENT.to_vec());
    }
    // shares ≤ total_shares, so the amount is at most the assets
    let amount = mul_div(shares, assets, state.total_shares).ok_or_else(overflow)?;
    if amount == 0 {
        return Err(ERR_TOO_SMALL.to_vec());
    }
    if amount > state.liquidity() {
        return Err(ERR_INSUFFICIENT_LIQUIDITY.to_vec());
    }
    state.cash = state.cash.checked_sub(amount).ok_or_else(overflow)?;
    state.total_shares = state
        .total_shares
        .checked_sub(shares)
        .ok_or_else(overflow)?;
    storage::write(KEY_STATE, &state);
    write_amount(&key, remaining);
    sdk::asset_transfer(&config.loan_asset, &account, amount);
    sdk::emit(&("withdraw", account, shares, amount));
    Ok(amount)
}

/// `deposit_collateral() -> collateral`: takes the attached collateral asset as the caller's collateral.
pub fn deposit_collateral() -> Result<u128, Vec<u8>> {
    let config = config()?;
    let amount = attached_only(&config.collateral_asset, ERR_ATTACH_COLLATERAL)?;
    let mut state = accrued_state(&config.params)?;
    let account = sdk::caller();
    let key = account_key(PREFIX_COLLATERAL, &account);
    let collateral = read_amount(&key)?
        .checked_add(amount)
        .ok_or_else(overflow)?;
    state.total_collateral = state
        .total_collateral
        .checked_add(amount)
        .ok_or_else(overflow)?;
    storage::write(KEY_STATE, &state);
    write_amount(&key, collateral);
    sdk::emit(&("deposit_collateral", account, amount));
    Ok(collateral)
}

/// `withdraw_collateral(amount) -> remaining`: returns collateral, keeping the caller's debt within its borrow limit.
pub fn withdraw_collateral(amount: u128) -> Result<u128, Vec<u8>> {
    let config = config()?;
    no_funds()?;
    if amount == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    let account = sdk::caller();
    let key = account_key(PREFIX_COLLATERAL, &account);
    let remaining = read_amount(&key)?
        .checked_sub(amount)
        .ok_or_else(|| ERR_INSUFFICIENT_COLLATERAL.to_vec())?;
    let mut state = accrued_state(&config.params)?;
    let debt = debt_of(&account, state.borrow_index)?;
    let (borrow_limit, _) = limits(remaining, price()?, &config.params)?;
    if debt > borrow_limit {
        return Err(ERR_UNHEALTHY.to_vec());
    }
    state.total_collateral = state
        .total_collateral
        .checked_sub(amount)
        .ok_or_else(overflow)?;
    storage::write(KEY_STATE, &state);
    write_amount(&key, remaining);
    sdk::asset_transfer(&config.collateral_asset, &account, amount);
    sdk::emit(&("withdraw_collateral", account, amount));
    Ok(remaining)
}

/// `borrow(amount) -> debt`: lends the caller `amount` of the loan asset within its borrow limit.
pub fn borrow(amount: u128) -> Result<u128, Vec<u8>> {
    let config = config()?;
    no_funds()?;
    if amount == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    let mut state = accrued_state(&config.params)?;
    if amount > state.liquidity() {
        return Err(ERR_INSUFFICIENT_LIQUIDITY.to_vec());
    }
    let account = sdk::caller();
    let debt = debt_of(&account, state.borrow_index)?
        .checked_add(amount)
        .ok_or_else(overflow)?;
    let collateral = read_amount(&account_key(PREFIX_COLLATERAL, &account))?;
    let (borrow_limit, _) = limits(collateral, price()?, &config.params)?;
    if debt > borrow_limit {
        return Err(ERR_BORROW_LIMIT.to_vec());
    }
    state.cash = state.cash.checked_sub(amount).ok_or_else(overflow)?;
    state.total_borrows = state
        .total_borrows
        .checked_add(amount)
        .ok_or_else(overflow)?;
    storage::write(KEY_STATE, &state);
    write_debt(&account, debt, state.borrow_index);
    sdk::asset_transfer(&config.loan_asset, &account, amount);
    sdk::emit(&("borrow", account, amount));
    Ok(debt)
}

/// `repay(borrower) -> remaining debt`: repays `borrower`'s debt with the attached loan asset, refunding the excess.
pub fn repay(borrower: &[u8; 32]) -> Result<u128, Vec<u8>> {
    let config = config()?;
    let attached = attached_only(&config.loan_asset, ERR_ATTACH_LOAN)?;
    let mut state = accrued_state(&config.params)?;
    let debt = debt_of(borrower, state.borrow_index)?;
    if debt == 0 {
        return Err(ERR_NO_DEBT.to_vec());
    }
    let paid = attached.min(debt);
    let remaining = debt.checked_sub(paid).ok_or_else(overflow)?;
    let refund = attached.checked_sub(paid).ok_or_else(overflow)?;
    state.cash = state.cash.checked_add(paid).ok_or_else(overflow)?;
    // The debts are rounded up and total_borrows down, so the debts may add up to more than total_borrows
    state.total_borrows = state.total_borrows.saturating_sub(paid);
    storage::write(KEY_STATE, &state);
    write_debt(borrower, remaining, state.borrow_index);
    let payer = sdk::caller();
    if refund > 0 {
        sdk::asset_transfer(&config.loan_asset, &payer, refund);
    }
    sdk::emit(&("repay", payer, *borrower, paid));
    Ok(remaining)
}

/// `liquidate(borrower) -> (repaid, seized)`: repays up to half of an unhealthy debt with the attached loan asset for
/// the borrower's collateral at a bonus, refunding the excess; writes off what the collateral cannot cover.
pub fn liquidate(borrower: &[u8; 32]) -> Result<(u128, u128), Vec<u8>> {
    let config = config()?;
    let params = &config.params;
    let attached = attached_only(&config.loan_asset, ERR_ATTACH_LOAN)?;
    let liquidator = sdk::caller();
    if *borrower == liquidator {
        return Err(ERR_SELF_LIQUIDATION.to_vec());
    }
    let mut state = accrued_state(params)?;
    let debt = debt_of(borrower, state.borrow_index)?;
    let collateral_key = account_key(PREFIX_COLLATERAL, borrower);
    let collateral = read_amount(&collateral_key)?;
    let price = price()?;
    let (_, liquidation_limit) = limits(collateral, price, params)?;
    if debt <= liquidation_limit {
        return Err(ERR_HEALTHY.to_vec());
    }
    // Close factor 50%: at most half of the debt, rounded up
    let repaid = attached.min(debt.div_ceil(2));
    let with_bonus = BPS
        .checked_add(params.liquidation_bonus_bps)
        .ok_or_else(overflow)?;
    // repaid ≤ 2^127 and the bonus is at most 20%, so the value fits
    let value = mul_div(repaid, u128::from(with_bonus), u128::from(BPS)).ok_or_else(overflow)?;
    // The price is never 0: a seizure above u128::MAX is certainly above the collateral
    let seized = mul_div(value, WAD, price).map_or(collateral, |seized| seized.min(collateral));
    let mut remaining = debt.checked_sub(repaid).ok_or_else(overflow)?;
    let left = collateral.checked_sub(seized).ok_or_else(overflow)?;
    let written_off = if left == 0 { remaining } else { 0 };
    remaining = remaining.checked_sub(written_off).ok_or_else(overflow)?;
    state.cash = state.cash.checked_add(repaid).ok_or_else(overflow)?;
    state.total_borrows = state
        .total_borrows
        .saturating_sub(repaid)
        .saturating_sub(written_off);
    state.total_collateral = state
        .total_collateral
        .checked_sub(seized)
        .ok_or_else(overflow)?;
    let refund = attached.checked_sub(repaid).ok_or_else(overflow)?;
    storage::write(KEY_STATE, &state);
    write_debt(borrower, remaining, state.borrow_index);
    write_amount(&collateral_key, left);
    if seized > 0 {
        sdk::asset_transfer(&config.collateral_asset, &liquidator, seized);
    }
    if refund > 0 {
        sdk::asset_transfer(&config.loan_asset, &liquidator, refund);
    }
    sdk::emit(&("liquidate", liquidator, *borrower, repaid, seized));
    if written_off > 0 {
        sdk::emit(&("write_off", *borrower, written_off));
    }
    Ok((repaid, seized))
}

/// `set_price(price)` (admin only): the collateral's price in the loan asset, times `WAD`.
pub fn set_price(price: u128) -> Result<(), Vec<u8>> {
    let config = config()?;
    no_funds()?;
    only_admin()?;
    if price == 0 {
        return Err(ERR_ZERO_PRICE.to_vec());
    }
    // Interest depends on time, not on the price; accruing first keeps the order of every method the same
    storage::write(KEY_STATE, &accrued_state(&config.params)?);
    storage::write(KEY_PRICE, &price);
    sdk::emit(&("price", price));
    Ok(())
}

/// `set_admin(admin)` (admin only): hands the role on; `None` leaves the market without an admin for good.
pub fn set_admin(new_admin: Option<[u8; 32]>) -> Result<(), Vec<u8>> {
    let config = config()?;
    no_funds()?;
    let previous = only_admin()?;
    if new_admin == Some([0u8; 32]) {
        return Err(ERR_ZERO_ACCOUNT.to_vec());
    }
    storage::write(KEY_STATE, &accrued_state(&config.params)?);
    match new_admin {
        Some(admin) => storage::write(KEY_ADMIN, &admin),
        None => storage::remove(KEY_ADMIN),
    }
    sdk::emit(&("admin", Some(previous), new_admin));
    Ok(())
}

/// `withdraw_reserves(amount, to)` (admin only): pays reserves out to `to`.
pub fn withdraw_reserves(amount: u128, to: &[u8; 32]) -> Result<(), Vec<u8>> {
    let config = config()?;
    no_funds()?;
    only_admin()?;
    if amount == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    if *to == [0u8; 32] {
        return Err(ERR_ZERO_ACCOUNT.to_vec());
    }
    let mut state = accrued_state(&config.params)?;
    if amount > state.reserves {
        return Err(ERR_EXCEEDS_RESERVES.to_vec());
    }
    if amount > state.cash {
        return Err(ERR_INSUFFICIENT_CASH.to_vec());
    }
    state.reserves = state.reserves.checked_sub(amount).ok_or_else(overflow)?;
    state.cash = state.cash.checked_sub(amount).ok_or_else(overflow)?;
    storage::write(KEY_STATE, &state);
    sdk::asset_transfer(&config.loan_asset, to, amount);
    sdk::emit(&("withdraw_reserves", *to, amount));
    Ok(())
}

/// `market() -> Market`: the market with interest accrued to the current block's time (read only).
pub fn market() -> Result<Market, Vec<u8>> {
    let config = config()?;
    no_funds()?;
    let params = config.params;
    let state = accrued_state(&params)?;
    let utilization = state.utilization_bps()?;
    let borrow_rate = borrow_rate_bps(&params, utilization)?;
    Ok(Market {
        loan_asset: config.loan_asset,
        collateral_asset: config.collateral_asset,
        admin: admin()?,
        price: price()?,
        params,
        cash: state.cash,
        total_borrows: state.total_borrows,
        reserves: state.reserves,
        total_shares: state.total_shares,
        total_collateral: state.total_collateral,
        borrow_index: state.borrow_index,
        last_accrual: state.last_accrual,
        utilization_bps: utilization,
        borrow_rate_bps: borrow_rate,
        supply_rate_bps: supply_rate_bps(borrow_rate, utilization, params.reserve_factor_bps)?,
    })
}

/// `position(account) -> Position`: `account`'s supply, collateral and debt with interest accrued to the current
/// block's time (read only).
pub fn position(account: &[u8; 32]) -> Result<Position, Vec<u8>> {
    let config = config()?;
    no_funds()?;
    let state = accrued_state(&config.params)?;
    let shares = read_amount(&account_key(PREFIX_SHARES, account))?;
    let supplied = if shares == 0 {
        0
    } else {
        mul_div(shares, state.total_assets()?, state.total_shares).ok_or_else(overflow)?
    };
    let collateral = read_amount(&account_key(PREFIX_COLLATERAL, account))?;
    let debt = debt_of(account, state.borrow_index)?;
    let (borrow_limit, liquidation_limit) = limits(collateral, price()?, &config.params)?;
    // The debt is not 0 here: None only when the health is above u128::MAX
    let health_bps =
        (debt > 0).then(|| mul_div(liquidation_limit, u128::from(BPS), debt).unwrap_or(u128::MAX));
    Ok(Position {
        shares,
        supplied,
        collateral,
        debt,
        borrow_limit,
        liquidation_limit,
        health_bps,
    })
}
