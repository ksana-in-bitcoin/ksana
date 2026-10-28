//! Example contract: a constant-product AMM using integer arithmetic (SPEC §9.5).
//!
//! The trading pair consists of the two sides given to `init`; each side is an [`AssetRef`] (`docs/STANDARDS.md` §5.3):
//!
//! - `Token(contract ID)`: a KS-20 token (`docs/STANDARDS.md` §3), or a token written before KS-20 with the same
//!   `transfer`, `transfer_from` and `balance_of`. It comes in one of two ways:
//!   - in one step, with the token's own `transfer_and_call(this contract, amount, data)`: the token moves the amount to
//!     this contract and calls its `on_token_received(from, amount, data)`, whose `data` is a [`PoolAction`], a swap or a
//!     deposit (`docs/STANDARDS.md` §3.3);
//!   - pulled by `swap` and `add_liquidity` with the token's `transfer_from` (caller → this contract), for which the caller
//!     must `approve` this contract beforehand (D-016).
//!
//!   It goes out with the token's `transfer` (this contract → recipient).
//! - `Asset(asset ID)`: a VM built-in asset (KSA or an asset a contract issued with `asset_issue`, D-073). Funds come in
//!   **attached** to the call (the `funds` of `Call`, or of `transfer_and_call`, which the token passes on) and go out
//!   with `asset_transfer`. The attached amount must be exactly the amount in the arguments; when a deposit uses only
//!   part of it proportionally, the excess is refunded to the sender. Attaching an asset outside the pair, or attaching
//!   funds for a token side, reverts.
//!
//! The reserves are kept in this contract's storage rather than read from balances; assets transferred directly to
//! this contract do not affect the price.
//!
//! Everything is integer arithmetic and any overflow returns an error (revert); intermediate products must fit in u128
//! (for example the first deposit requires `amount_a · amount_b < 2^128`).
//!
//! Arguments and return values are Borsh-encoded. The arguments are a tuple in the order of the table below (a single
//! argument is just itself; no arguments is empty bytes):
//!
//! | Method | Arguments | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `(side_a: AssetRef, side_b: AssetRef)` | `()` |
//! | `add_liquidity` | `(amount_a: u128, amount_b: u128, min_shares: u128)` | `u128` (shares received) |
//! | `remove_liquidity` | `(shares: u128, min_a: u128, min_b: u128)` | `(u128, u128)` (amounts of a and b withdrawn) |
//! | `swap` | `(id_in: [u8; 32], amount_in: u128, min_out: u128)` | `u128` (amount received) |
//! | `on_token_received` (by a token side) | `(from: [u8; 32], amount: u128, data: Vec<u8>)` | `Vec<u8>`: the Borsh `u128` paid out or the shares |
//! | `reserves` | none | `(u128, u128)` (reserves of a and b) |
//! | `shares_of` | `account: [u8; 32]` | `u128` |
//! | `total_shares` | none | `u128` |
//! | `tokens` | none | `(AssetRef, AssetRef)` |
//!
//! Borsh encoding of `AssetRef`: a 1-byte variant (0 = `Token`, 1 = `Asset`) + a 32-byte ID, the `Side` of this contract
//! before KS-20. The IDs of the two sides must differ; `swap` names the input side by its ID. Borsh encoding of
//! [`PoolAction`]: a 1-byte variant (0 = `Swap`, 1 = `AddLiquidity`) + its `u128` fields.
//!
//! Callers written before the minimums of `add_liquidity` and `remove_liquidity` may leave them out: `(amount_a,
//! amount_b)` and a bare `shares` are taken with minimums of 0.
//!
//! - `add_liquidity`: on the first deposit (total shares 0), shares = `isqrt(amount_a · amount_b)` (Newton iteration)
//!   and both amounts are taken in full; afterwards shares = `min(amount_a · S / R_a, amount_b · S / R_b)` (rounded
//!   down) and only the amounts needed at the current ratio, `ceil(shares · R / S)` (never more than the amounts
//!   given), are taken; a token side pulled is pulled at that amount, and the rest of an asset side, or of the tokens
//!   sent with `transfer_and_call`, is refunded. Reverts when the shares are 0 or below `min_shares`. A swap just
//!   before a deposit moves the ratio it enters at. Amounts in the pool's ratio get fewer shares whichever way the
//!   ratio moves, so offer amounts in that ratio, with `min_shares` a little below the shares a read-only call of the
//!   deposit returns.
//! - `remove_liquidity`: withdraws `shares · R / S` (rounded down); reverts when either amount is 0 or below its
//!   minimum.
//! - `swap`: charges a 0.3% fee, `out = amount_in·997·R_out / (R_in·1000 + amount_in·997)`; reverts when `out == 0`
//!   or `out < min_out`.
//! - `on_token_received(from, amount, data)`: only a token side of this pool may call it (anyone can call it directly
//!   with a made-up `from` and `amount`; only the token itself makes them true), and the tokens it received are
//!   `amount`. `data` is a [`PoolAction`]:
//!   - `Swap(min_out)`: swaps `amount` as `swap` does and pays the other side to `from`; takes no attached funds;
//!   - `AddLiquidity(amount_other, min_shares)`: deposits `amount` of this side and up to `amount_other` of the other
//!     side for `from`, as `add_liquidity` does: a built-in asset attached exactly (to `transfer_and_call`, which the
//!     token passes on), a token pulled from `from` with `transfer_from` (approved beforehand), and the tokens of this
//!     side that the deposit does not use sent back to `from`.
//!
//!   It answers with the Borsh `u128` (the amount paid out or the shares) inside the `Bytes` of the standard, which
//!   `transfer_and_call` returns unchanged.
//!
//! A token that charges a fee credits this contract with less than the amount sent (`docs/STANDARDS.md` KS-20 rule 4):
//! sent with `transfer_and_call`, it trades and deposits at the amount received; pulled, it would leave the pool
//! holding less than it records, so after every pull of a token side this contract checks that it holds at least its
//! reserve of that side and otherwise reverts (`received less than the amount`). The fee of a token this contract pays
//! out comes out of what the recipient gets, as it does for every sender.
//!
//! Reserves and shares are updated before the transfers (checks-effects-interactions); any failing token call fails
//! the whole operation. The pool trusts its token sides as any holder of a token trusts it: a token can lie about its
//! balances and its transfers whichever way it is sent, so `on_token_received` adds no trust beyond the pull.
//!
//! Events (Borsh): `("add_liquidity", provider, amount_a, amount_b, shares)`,
//! `("remove_liquidity", provider, shares, amount_a, amount_b)`,
//! `("swap", trader, id_in, amount_in, amount_out)`; with `on_token_received` the provider or the trader is `from`.
//!
//! The same interface in the playground's ABI notation (`playground/README.md`), which the playground reads to build its
//! forms and decode results and events:
//!
//! ```abi
//! enum AssetRef { Token(ContractId), Asset(AssetId) }
//! enum PoolAction { Swap(u128), AddLiquidity(u128, u128) }
//! init(side_a: AssetRef, side_b: AssetRef)
//! payable call add_liquidity(amount_a: u128, amount_b: u128, min_shares: u128) -> u128   // attach the built-in asset sides, approve the token sides; returns the shares received
//! call remove_liquidity(shares: u128, min_a: u128, min_b: u128) -> (u128, u128)          // returns the amounts of a and b withdrawn
//! payable call swap(id_in: Hash, amount_in: u128, min_out: u128) -> u128                 // id_in names the input side; attach it when it is an asset, approve it when it is a token
//! payable call on_token_received(from: Account, amount: u128, data: Bytes) -> Bytes      // a token side's transfer_and_call calls it, data a PoolAction; called directly it reverts
//! view reserves() -> (u128, u128)
//! view shares_of(account: Account) -> u128
//! view total_shares() -> u128
//! view tokens() -> (AssetRef, AssetRef)
//! event add_liquidity(provider: Account, amount_a: u128, amount_b: u128, shares: u128)
//! event remove_liquidity(provider: Account, shares: u128, amount_a: u128, amount_b: u128)
//! event swap(trader: Account, id_in: Hash, amount_in: u128, amount_out: u128)
//! ```

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::vec::Vec;

use ksana_contract_sdk::borsh::BorshSerialize;
use ksana_contract_sdk::ks20::{self, Received};
use ksana_contract_sdk::{self as sdk, standard, storage, CallResult, Decode};

pub use ksana_contract_sdk::standard::AssetRef;

sdk::entry!(dispatch);

const KEY_TOKENS: &[u8] = b"tokens";
const KEY_RESERVES: &[u8] = b"reserves";
const KEY_TOTAL_SHARES: &[u8] = b"total_shares";
const PREFIX_SHARES: &[u8] = b"s";

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_SAME_TOKEN: &[u8] = b"token_a and token_b must differ";
const ERR_ZERO_AMOUNT: &[u8] = b"amount must be positive";
const ERR_ZERO_SHARES: &[u8] = b"deposit too small";
const ERR_INSUFFICIENT_SHARES: &[u8] = b"insufficient shares";
const ERR_ZERO_OUTPUT: &[u8] = b"output amount is zero";
const ERR_NO_LIQUIDITY: &[u8] = b"no liquidity";
const ERR_UNKNOWN_TOKEN: &[u8] = b"token is not in this pool";
const ERR_SLIPPAGE: &[u8] = b"output below min_out";
const ERR_MIN_SHARES: &[u8] = b"shares below min_shares";
const ERR_MIN_AMOUNTS: &[u8] = b"output below min_a or min_b";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";
const ERR_UNEXPECTED_FUNDS: &[u8] = b"unexpected attached funds";
const ERR_FUNDS_MISMATCH: &[u8] = b"attached funds must equal the amount";
const ERR_NOT_A_SIDE: &[u8] = b"caller is not a token of this pool";
const ERR_NOT_AN_ACTION: &[u8] = b"data is not a pool action";
const ERR_RECEIVED_LESS: &[u8] = b"received less than the amount";

/// What tokens sent with `transfer_and_call` are for: the `data` of `on_token_received`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub enum PoolAction {
    /// `Swap(min_out)`: swap the tokens for the other side, paying out at least `min_out`.
    Swap(u128),
    /// `AddLiquidity(amount_other, min_shares)`: deposit the tokens and up to `amount_other` of the other side, for at
    /// least `min_shares`.
    AddLiquidity(u128, u128),
}

impl Decode for PoolAction {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(PoolAction::Swap(u128::decode(input)?)),
            1 => Some(PoolAction::AddLiquidity(
                u128::decode(input)?,
                u128::decode(input)?,
            )),
            _ => None,
        }
    }
}

/// Pure integer arithmetic (no host access); `None` means overflow or division by zero.
pub mod math {
    /// `floor(sqrt(n))` by Newton iteration (a loop, no recursion).
    ///
    /// The initial value `2^(floor(log2 n)/2 + 1)` is greater than `sqrt(n)`; every step after that strictly decreases
    /// until `floor(sqrt(n))` is reached.
    pub fn isqrt(n: u128) -> u128 {
        if n < 2 {
            return n;
        }
        // n.ilog2() <= 127, so the exponent is <= 64 and neither the initial value nor the sum in any step overflows.
        let Some(mut x) = 1u128.checked_shl((n.ilog2() >> 1).saturating_add(1)) else {
            return 0;
        };
        loop {
            let Some(y) = n
                .checked_div(x)
                .and_then(|q| q.checked_add(x))
                .map(|sum| sum >> 1)
            else {
                return x;
            };
            if y >= x {
                return x;
            }
            x = y;
        }
    }

    /// Division rounding up.
    fn ceil_div(numerator: u128, denominator: u128) -> Option<u128> {
        let quotient = numerator.checked_div(denominator)?;
        if numerator.checked_rem(denominator)? == 0 {
            Some(quotient)
        } else {
            quotient.checked_add(1)
        }
    }

    /// Swap output (0.3% fee): `amount_in·997·reserve_out / (reserve_in·1000 + amount_in·997)`.
    pub fn swap_output(amount_in: u128, reserve_in: u128, reserve_out: u128) -> Option<u128> {
        let in_with_fee = amount_in.checked_mul(997)?;
        let numerator = in_with_fee.checked_mul(reserve_out)?;
        let denominator = reserve_in.checked_mul(1000)?.checked_add(in_with_fee)?;
        numerator.checked_div(denominator)
    }

    /// Adding liquidity: returns `(shares, a actually pulled, b actually pulled)`.
    ///
    /// When the total shares are 0 the shares are `isqrt(amount_a · amount_b)` and everything is pulled; otherwise the
    /// shares are the smaller of the two sides' ratios, and `ceil(shares · R / S)` is actually pulled (never more than
    /// the amounts given, and without diluting existing shares).
    pub fn deposit(
        amount_a: u128,
        amount_b: u128,
        reserve_a: u128,
        reserve_b: u128,
        total_shares: u128,
    ) -> Option<(u128, u128, u128)> {
        if total_shares == 0 {
            let shares = isqrt(amount_a.checked_mul(amount_b)?);
            return Some((shares, amount_a, amount_b));
        }
        let shares_a = amount_a.checked_mul(total_shares)?.checked_div(reserve_a)?;
        let shares_b = amount_b.checked_mul(total_shares)?.checked_div(reserve_b)?;
        let shares = shares_a.min(shares_b);
        let used_a = ceil_div(shares.checked_mul(reserve_a)?, total_shares)?;
        let used_b = ceil_div(shares.checked_mul(reserve_b)?, total_shares)?;
        Some((shares, used_a, used_b))
    }

    /// Removing liquidity: `(shares · R_a / S, shares · R_b / S)`, rounded down.
    pub fn withdraw(
        shares: u128,
        reserve_a: u128,
        reserve_b: u128,
        total_shares: u128,
    ) -> Option<(u128, u128)> {
        let out_a = shares.checked_mul(reserve_a)?.checked_div(total_shares)?;
        let out_b = shares.checked_mul(reserve_b)?.checked_div(total_shares)?;
        Some((out_a, out_b))
    }
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let (side_a, side_b) = sdk::decode(args)?;
            init(side_a, side_b)?;
            sdk::ok(&())
        }
        "add_liquidity" => {
            let (amount_a, amount_b, min_shares) = match sdk::decode(args) {
                Ok(with_minimum) => with_minimum,
                // Without the minimum, as callers written before it send them
                Err(_) => {
                    let (amount_a, amount_b) = sdk::decode(args)?;
                    (amount_a, amount_b, 0)
                }
            };
            sdk::ok(&add_liquidity(amount_a, amount_b, min_shares)?)
        }
        "remove_liquidity" => {
            let (shares, min_a, min_b) = match sdk::decode(args) {
                Ok(with_minimums) => with_minimums,
                // A bare `shares`, as callers written before the minimums send it
                Err(_) => (sdk::decode(args)?, 0, 0),
            };
            sdk::ok(&remove_liquidity(shares, min_a, min_b)?)
        }
        "swap" => {
            let (id_in, amount_in, min_out) = sdk::decode(args)?;
            sdk::ok(&swap(&id_in, amount_in, min_out)?)
        }
        "on_token_received" => standard::reply(&on_token_received(&ks20::received(args)?)?),
        "reserves" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&reserves()?)
        }
        "shares_of" => {
            let account = sdk::decode(args)?;
            sdk::ok(&shares_of(&account)?)
        }
        "total_shares" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&total_shares()?)
        }
        "tokens" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&tokens()?)
        }
        _ => sdk::revert("unknown method"),
    }
}

fn overflow() -> Vec<u8> {
    ERR_OVERFLOW.to_vec()
}

fn shares_key(account: &[u8; 32]) -> Vec<u8> {
    [PREFIX_SHARES, account.as_slice()].concat()
}

/// `init(side_a, side_b)`.
pub fn init(side_a: AssetRef, side_b: AssetRef) -> Result<(), Vec<u8>> {
    if storage::get(KEY_TOKENS).is_some() {
        return Err(ERR_ALREADY_INITIALIZED.to_vec());
    }
    if side_a.id() == side_b.id() {
        return Err(ERR_SAME_TOKEN.to_vec());
    }
    // Funds attached at deployment have nowhere to be accounted for; reject them
    if !sdk::call_funds().is_empty() {
        return Err(ERR_UNEXPECTED_FUNDS.to_vec());
    }
    storage::write(KEY_TOKENS, &(side_a, side_b));
    storage::write(KEY_RESERVES, &(0u128, 0u128));
    storage::write(KEY_TOTAL_SHARES, &0u128);
    Ok(())
}

/// `tokens() -> (side_a, side_b)`.
pub fn tokens() -> Result<(AssetRef, AssetRef), Vec<u8>> {
    storage::read(KEY_TOKENS)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

/// `reserves() -> (reserve_a, reserve_b)`.
pub fn reserves() -> Result<(u128, u128), Vec<u8>> {
    storage::read(KEY_RESERVES)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

/// `total_shares()`.
pub fn total_shares() -> Result<u128, Vec<u8>> {
    storage::read(KEY_TOTAL_SHARES)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

/// `shares_of(account)`.
pub fn shares_of(account: &[u8; 32]) -> Result<u128, Vec<u8>> {
    Ok(storage::read(&shares_key(account))?.unwrap_or(0))
}

fn write_shares(account: &[u8; 32], shares: u128) {
    let key = shares_key(account);
    if shares == 0 {
        storage::remove(&key);
    } else {
        storage::write(&key, &shares);
    }
}

/// Checks the attached funds: only the asset of a built-in asset side of the pair may be attached; returns the amount
/// attached for each side (0 for a token side).
fn attached(side_a: AssetRef, side_b: AssetRef) -> Result<(u128, u128), Vec<u8>> {
    let (mut a, mut b) = (0u128, 0u128);
    for f in sdk::call_funds() {
        match (side_a, side_b) {
            (AssetRef::Asset(id), _) if id == f.asset => a = f.amount,
            (_, AssetRef::Asset(id)) if id == f.asset => b = f.amount,
            _ => return Err(ERR_UNEXPECTED_FUNDS.to_vec()),
        }
    }
    Ok((a, b))
}

/// Takes `amount` of a side from `from`: a token side calls `token.transfer_from(from, this contract, amount)` and then
/// checks that this contract holds at least `reserve`, its new reserve of the side (a token that charges a fee credits
/// less than the amount); a built-in asset side requires the attached amount to be exactly `declared`, uses `amount`
/// of it and refunds the excess to `from`.
fn pull(
    side: AssetRef,
    from: &[u8; 32],
    attached: u128,
    declared: u128,
    amount: u128,
    reserve: u128,
) -> Result<(), Vec<u8>> {
    match side {
        AssetRef::Token(token) => {
            let token = ks20::Token(token);
            let this = sdk::self_id();
            token.transfer_from(from, &this, amount);
            if token.balance_of(&this)? < reserve {
                return Err(ERR_RECEIVED_LESS.to_vec());
            }
        }
        AssetRef::Asset(asset) => {
            if attached != declared {
                return Err(ERR_FUNDS_MISMATCH.to_vec());
            }
            let refund = declared.checked_sub(amount).ok_or_else(overflow)?;
            if refund > 0 {
                sdk::asset_transfer(&asset, from, refund);
            }
        }
    }
    Ok(())
}

/// A deposit as [`deposit`] wrote it.
struct Deposit {
    shares: u128,
    used_a: u128,
    used_b: u128,
    /// The new reserves
    reserve_a: u128,
    reserve_b: u128,
}

/// The effects of a deposit of up to `amount_a` and `amount_b` for `provider`: the shares and the amounts used
/// ([`math::deposit`]), checked against `min_shares`, are written to the reserves and the provider's shares.
fn deposit(
    provider: &[u8; 32],
    amount_a: u128,
    amount_b: u128,
    min_shares: u128,
) -> Result<Deposit, Vec<u8>> {
    if amount_a == 0 || amount_b == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    let (reserve_a, reserve_b) = reserves()?;
    let total = total_shares()?;
    let (shares, used_a, used_b) =
        math::deposit(amount_a, amount_b, reserve_a, reserve_b, total).ok_or_else(overflow)?;
    if shares == 0 {
        return Err(ERR_ZERO_SHARES.to_vec());
    }
    if shares < min_shares {
        return Err(ERR_MIN_SHARES.to_vec());
    }
    let new_reserves = (
        reserve_a.checked_add(used_a).ok_or_else(overflow)?,
        reserve_b.checked_add(used_b).ok_or_else(overflow)?,
    );
    let new_total = total.checked_add(shares).ok_or_else(overflow)?;
    let new_held = shares_of(provider)?
        .checked_add(shares)
        .ok_or_else(overflow)?;
    storage::write(KEY_RESERVES, &new_reserves);
    storage::write(KEY_TOTAL_SHARES, &new_total);
    write_shares(provider, new_held);
    Ok(Deposit {
        shares,
        used_a,
        used_b,
        reserve_a: new_reserves.0,
        reserve_b: new_reserves.1,
    })
}

/// The effects of a swap of `amount_in` (from side a to side b when `a_to_b`): the output, checked against `min_out`,
/// and the new reserves are written; returns `(amount_out, the new reserve of the input side)`.
fn trade(a_to_b: bool, amount_in: u128, min_out: u128) -> Result<(u128, u128), Vec<u8>> {
    let (reserve_a, reserve_b) = reserves()?;
    let (reserve_in, reserve_out) = if a_to_b {
        (reserve_a, reserve_b)
    } else {
        (reserve_b, reserve_a)
    };
    if reserve_in == 0 || reserve_out == 0 {
        return Err(ERR_NO_LIQUIDITY.to_vec());
    }
    let amount_out = math::swap_output(amount_in, reserve_in, reserve_out).ok_or_else(overflow)?;
    if amount_out == 0 {
        return Err(ERR_ZERO_OUTPUT.to_vec());
    }
    if amount_out < min_out {
        return Err(ERR_SLIPPAGE.to_vec());
    }
    let new_in = reserve_in.checked_add(amount_in).ok_or_else(overflow)?;
    let new_out = reserve_out.checked_sub(amount_out).ok_or_else(overflow)?;
    let new_reserves = if a_to_b {
        (new_in, new_out)
    } else {
        (new_out, new_in)
    };
    storage::write(KEY_RESERVES, &new_reserves);
    Ok((amount_out, new_in))
}

/// `add_liquidity(amount_a, amount_b, min_shares) -> shares`.
pub fn add_liquidity(amount_a: u128, amount_b: u128, min_shares: u128) -> Result<u128, Vec<u8>> {
    if amount_a == 0 || amount_b == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    let (side_a, side_b) = tokens()?;
    let (attached_a, attached_b) = attached(side_a, side_b)?;
    let provider = sdk::caller();
    let d = deposit(&provider, amount_a, amount_b, min_shares)?;
    pull(
        side_a,
        &provider,
        attached_a,
        amount_a,
        d.used_a,
        d.reserve_a,
    )?;
    pull(
        side_b,
        &provider,
        attached_b,
        amount_b,
        d.used_b,
        d.reserve_b,
    )?;
    sdk::emit(&("add_liquidity", provider, d.used_a, d.used_b, d.shares));
    Ok(d.shares)
}

/// `remove_liquidity(shares, min_a, min_b) -> (amount_a, amount_b)`.
pub fn remove_liquidity(shares: u128, min_a: u128, min_b: u128) -> Result<(u128, u128), Vec<u8>> {
    if shares == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    if !sdk::call_funds().is_empty() {
        return Err(ERR_UNEXPECTED_FUNDS.to_vec());
    }
    let provider = sdk::caller();
    let held = shares_of(&provider)?;
    let remaining = held
        .checked_sub(shares)
        .ok_or_else(|| ERR_INSUFFICIENT_SHARES.to_vec())?;
    let (side_a, side_b) = tokens()?;
    let (reserve_a, reserve_b) = reserves()?;
    let total = total_shares()?;
    let (out_a, out_b) =
        math::withdraw(shares, reserve_a, reserve_b, total).ok_or_else(overflow)?;
    if out_a == 0 || out_b == 0 {
        return Err(ERR_ZERO_OUTPUT.to_vec());
    }
    if out_a < min_a || out_b < min_b {
        return Err(ERR_MIN_AMOUNTS.to_vec());
    }
    let new_reserves = (
        reserve_a.checked_sub(out_a).ok_or_else(overflow)?,
        reserve_b.checked_sub(out_b).ok_or_else(overflow)?,
    );
    let new_total = total.checked_sub(shares).ok_or_else(overflow)?;
    storage::write(KEY_RESERVES, &new_reserves);
    storage::write(KEY_TOTAL_SHARES, &new_total);
    write_shares(&provider, remaining);
    side_a.pay(&provider, out_a);
    side_b.pay(&provider, out_b);
    sdk::emit(&("remove_liquidity", provider, shares, out_a, out_b));
    Ok((out_a, out_b))
}

/// `swap(id_in, amount_in, min_out) -> amount_out`.
pub fn swap(id_in: &[u8; 32], amount_in: u128, min_out: u128) -> Result<u128, Vec<u8>> {
    if amount_in == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    let (side_a, side_b) = tokens()?;
    let (attached_a, attached_b) = attached(side_a, side_b)?;
    let a_to_b = if *id_in == side_a.id() {
        true
    } else if *id_in == side_b.id() {
        false
    } else {
        return Err(ERR_UNKNOWN_TOKEN.to_vec());
    };
    let (side_in, side_out, attached_in, attached_out) = if a_to_b {
        (side_a, side_b, attached_a, attached_b)
    } else {
        (side_b, side_a, attached_b, attached_a)
    };
    // Only the input side's asset may be attached
    if attached_out != 0 {
        return Err(ERR_UNEXPECTED_FUNDS.to_vec());
    }
    let (amount_out, reserve_in) = trade(a_to_b, amount_in, min_out)?;
    let trader = sdk::caller();
    pull(
        side_in,
        &trader,
        attached_in,
        amount_in,
        amount_in,
        reserve_in,
    )?;
    side_out.pay(&trader, amount_out);
    sdk::emit(&("swap", trader, *id_in, amount_in, amount_out));
    Ok(amount_out)
}

/// `on_token_received(from, amount, data) -> Bytes`: `amount` of a token side, sent by `from` with the token's
/// `transfer_and_call`, for the [`PoolAction`] in `data`; returns the amount paid out (`Swap`) or the shares
/// (`AddLiquidity`), which the dispatch function answers inside the standard's `Bytes`.
pub fn on_token_received(received: &Received) -> Result<u128, Vec<u8>> {
    let (side_a, side_b) = tokens()?;
    // Only the token itself makes `from` and `amount` true; anyone can call this method directly
    let token = AssetRef::Token(received.token);
    let a_in = if token == side_a {
        true
    } else if token == side_b {
        false
    } else {
        return Err(ERR_NOT_A_SIDE.to_vec());
    };
    let action: PoolAction = sdk::decode(&received.data).map_err(|_| ERR_NOT_AN_ACTION.to_vec())?;
    let (from, amount) = (received.from, received.amount);
    if amount == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    let (attached_a, attached_b) = attached(side_a, side_b)?;
    let (side_other, attached_other) = if a_in {
        (side_b, attached_b)
    } else {
        (side_a, attached_a)
    };
    match action {
        PoolAction::Swap(min_out) => {
            // The tokens are the input; nothing else comes in
            if attached_other != 0 {
                return Err(ERR_UNEXPECTED_FUNDS.to_vec());
            }
            let (amount_out, _) = trade(a_in, amount, min_out)?;
            side_other.pay(&from, amount_out);
            sdk::emit(&("swap", from, received.token, amount, amount_out));
            Ok(amount_out)
        }
        PoolAction::AddLiquidity(amount_other, min_shares) => {
            let (amount_a, amount_b) = if a_in {
                (amount, amount_other)
            } else {
                (amount_other, amount)
            };
            let d = deposit(&from, amount_a, amount_b, min_shares)?;
            let (used, used_other, reserve_other) = if a_in {
                (d.used_a, d.used_b, d.reserve_b)
            } else {
                (d.used_b, d.used_a, d.reserve_a)
            };
            let unused = amount.checked_sub(used).ok_or_else(overflow)?;
            if unused > 0 {
                token.pay(&from, unused);
            }
            pull(
                side_other,
                &from,
                attached_other,
                amount_other,
                used_other,
                reserve_other,
            )?;
            sdk::emit(&("add_liquidity", from, d.used_a, d.used_b, d.shares));
            Ok(d.shares)
        }
    }
}
