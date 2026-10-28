//! KS-20, the fungible token standard (`docs/STANDARDS.md`, D-120).
//!
//! - [`Token`]: calling a KS-20 token from another contract;
//! - [`received`]: what `transfer_and_call` gives a contract's `on_token_received`;
//! - [`Event`]: the standard's events, parsed and emitted;
//! - [`Ks20`]: the reference implementation a token contract is built on (the ledger, allowances with an optional
//!   expiry, `transfer_and_call`, the declaration, the extensions `KS-20-burnable` and `KS-20-mintable`), with the
//!   token's own rules in a [`Policy`].
//!
//! # A token
//!
//! ```ignore
//! #![cfg_attr(target_arch = "wasm32", no_std)]
//! extern crate alloc;
//!
//! use alloc::string::String;
//! use ksana_contract_sdk::{self as sdk, ks20::Ks20, standard, CallResult};
//!
//! sdk::entry!(dispatch);
//!
//! const TOKEN: Ks20 = Ks20::new().burnable();
//!
//! pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
//!     if method == "init" {
//!         standard::no_funds()?;
//!         let (name, symbol, decimals, supply): (String, String, u8, u128) = sdk::decode(args)?;
//!         TOKEN.init(&name, &symbol, decimals)?;
//!         TOKEN.mint(&sdk::caller(), supply)?;
//!         return sdk::ok(&());
//!     }
//!     TOKEN.dispatch(method, args).unwrap_or_else(|| sdk::revert("unknown method"))
//! }
//! ```
//!
//! A token with rules of its own implements [`Policy`] (a check before every movement, a fee, the capabilities it
//! reports) and builds on `Ks20::new().burnable().with_policy(rules)` (the extensions first); its own methods go in its
//! dispatch function beside [`Ks20::dispatch`], which answers `None` for every method it does not have. A method the
//! token answers itself before calling [`Ks20::dispatch`] replaces the implementation's.
//!
//! # Receiving tokens
//!
//! A contract that takes tokens in one step implements `on_token_received(from, amount, data) -> Bytes`: it checks
//! that the caller is a token it accepts, decodes the arguments with [`received`] and answers with
//! [`crate::standard::reply`]. `transfer_and_call` returns that answer unchanged: the same Borsh `Vec<u8>`, whose
//! content the receiving contract defines (a pool's swap, for example, the amount it paid out).
//!
//! # Storage
//!
//! The implementation keeps its state under keys that start with `KS20:`; a contract built on it keeps its own state
//! under other keys. Amounts are Borsh `u128`, absent at 0.

use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::{decode, encode, CallResult, Decode};
use crate::standard::{
    check_name, check_recipient, check_symbol, decode_return, expired, ordered, rejected, run,
    CAPABILITIES_VIEW, CAP_MINT, ERR_ALREADY_INITIALIZED, ERR_MINTING_ENDED, ERR_NOT_INITIALIZED,
    ERR_NOT_THE_MINTER, ERR_OVERFLOW, ERR_ZERO_ACCOUNT, METADATA, STANDARDS, ZERO_ACCOUNT,
};
use crate::{storage, CallError, Funds};

/// The standard's ID, the first name `standards()` lists.
pub const ID: &str = "KS-20";
/// The extension of `burn` and `burn_from`.
pub const BURNABLE: &str = "KS-20-burnable";
/// The extension of `mint`, `minter`, `set_minter` and `cap`.
pub const MINTABLE: &str = "KS-20-mintable";
/// The method `transfer_and_call` calls on the recipient.
pub const ON_RECEIVED: &str = "on_token_received";
/// The most decimals a token has.
pub const MAX_DECIMALS: u8 = 18;
/// An allowance of this amount is unlimited: `transfer_from` does not reduce it.
pub const UNLIMITED: u128 = u128::MAX;

/// A transfer, mint or burn of 0.
pub const ERR_ZERO_AMOUNT: &[u8] = b"zero amount";
/// The sender holds less than the amount.
pub const ERR_INSUFFICIENT_BALANCE: &[u8] = b"insufficient balance";
/// The spender's allowance is less than the amount (or there is none).
pub const ERR_INSUFFICIENT_ALLOWANCE: &[u8] = b"insufficient allowance";
/// The spender's allowance expired.
pub const ERR_ALLOWANCE_EXPIRED: &[u8] = b"allowance expired";
/// An account approving itself (`transfer_from` of one's own tokens needs no allowance).
pub const ERR_SPENDER_IS_CALLER: &[u8] = b"spender is the caller";
/// More than [`MAX_DECIMALS`] decimals.
pub const ERR_DECIMALS: &[u8] = b"decimals must be at most 18";
/// A mint past the cap.
pub const ERR_CAP_EXCEEDED: &[u8] = b"cap exceeded";
/// A mint past `u128::MAX` tokens in all.
pub const ERR_SUPPLY_OVERFLOW: &[u8] = b"supply overflow";
/// The reason, after [`crate::standard::REJECTED`], of a transfer whose fee is the whole amount or more.
pub const REASON_FEE: &str = "the fee takes the whole amount";

const KEY_METADATA: &[u8] = b"KS20:m";
const KEY_SUPPLY: &[u8] = b"KS20:s";
const PREFIX_BALANCE: &[u8] = b"KS20:b";
const PREFIX_ALLOWANCE: &[u8] = b"KS20:a";
const KEY_MINTER: &[u8] = b"KS20:M";
const KEY_CAP: &[u8] = b"KS20:c";

/// A KS-20 token, called from a contract. The calls that move tokens fail the whole op when the token refuses; the
/// `try_` ones return the refusal instead ([`crate::try_call`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token(pub [u8; 32]);

impl Token {
    fn view<T: Decode>(&self, method: &str, args: &[u8]) -> Result<T, Vec<u8>> {
        decode_return(&crate::call(&self.0, method, args))
    }

    /// `standards()`.
    pub fn standards(&self) -> Result<Vec<String>, Vec<u8>> {
        self.view(STANDARDS, &[])
    }

    /// `metadata() -> (name, symbol, decimals)`.
    pub fn metadata(&self) -> Result<(String, String, u8), Vec<u8>> {
        self.view(METADATA, &[])
    }

    /// `total_supply()`.
    pub fn total_supply(&self) -> Result<u128, Vec<u8>> {
        self.view("total_supply", &[])
    }

    /// `balance_of(account)`.
    pub fn balance_of(&self, account: &[u8; 32]) -> Result<u128, Vec<u8>> {
        self.view("balance_of", &encode(account))
    }

    /// `allowance(owner, spender)`: what `spender` may take from `owner` now (0 once the allowance expired).
    pub fn allowance(&self, owner: &[u8; 32], spender: &[u8; 32]) -> Result<u128, Vec<u8>> {
        self.view("allowance", &encode(&(*owner, *spender)))
    }

    /// `transfer(to, amount)` of this contract's tokens.
    pub fn transfer(&self, to: &[u8; 32], amount: u128) {
        crate::call(&self.0, "transfer", &encode(&(*to, amount)));
    }

    /// `transfer_from(from, to, amount)` with the allowance `from` gave this contract.
    pub fn transfer_from(&self, from: &[u8; 32], to: &[u8; 32], amount: u128) {
        crate::call(&self.0, "transfer_from", &encode(&(*from, *to, amount)));
    }

    /// `approve(spender, amount, expires_at)` of this contract's tokens.
    pub fn approve(&self, spender: &[u8; 32], amount: u128, expires_at: Option<u64>) {
        crate::call(&self.0, "approve", &encode(&(*spender, amount, expires_at)));
    }

    /// `transfer_and_call(to, amount, data)` of this contract's tokens, attaching `funds` from this contract's
    /// account (the token passes them on to `to`); returns the content of the `Bytes` that `to`'s `on_token_received`
    /// answered.
    pub fn transfer_and_call(
        &self,
        to: &[u8; 32],
        amount: u128,
        data: &[u8],
        funds: &[Funds],
    ) -> Result<Vec<u8>, Vec<u8>> {
        let args = encode(&(*to, amount, data));
        decode_return(&crate::call_with_funds(
            &self.0,
            "transfer_and_call",
            &args,
            funds,
        ))
    }

    /// [`Token::transfer`], returning the token's refusal instead of failing the op.
    pub fn try_transfer(&self, to: &[u8; 32], amount: u128) -> Result<(), CallError> {
        crate::try_call(&self.0, "transfer", &encode(&(*to, amount))).map(|_| ())
    }

    /// [`Token::transfer_from`], returning the token's refusal instead of failing the op.
    pub fn try_transfer_from(
        &self,
        from: &[u8; 32],
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), CallError> {
        crate::try_call(&self.0, "transfer_from", &encode(&(*from, *to, amount))).map(|_| ())
    }
}

/// What `transfer_and_call` gives the recipient's `on_token_received(from, amount, data)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    /// The token: the caller of `on_token_received`. Only when it is a token this contract accepts are `from` and
    /// `amount` true: anyone can call `on_token_received` directly with made-up values.
    pub token: [u8; 32],
    /// The account that sent the tokens (the caller of `transfer_and_call`).
    pub from: [u8; 32],
    /// The amount this contract received (the amount sent less the token's fee, if it charges one).
    pub amount: u128,
    /// The sender's data, which tells this contract what to do.
    pub data: Vec<u8>,
}

/// Decodes the arguments of `on_token_received` (`args` of the dispatch function). The funds the sender attached to
/// `transfer_and_call` are this call's attached funds ([`crate::call_funds`]). The method answers with
/// [`crate::standard::reply`].
pub fn received(args: &[u8]) -> Result<Received, Vec<u8>> {
    let (from, amount, data) = decode(args)?;
    Ok(Received {
        token: crate::caller(),
        from,
        amount,
        data,
    })
}

/// A KS-20 event: a Borsh tuple with the name first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// `("standards", ids)`: the declaration, the first event of `init`.
    Standards(Vec<String>),
    /// `("metadata", name, symbol, decimals)`: emitted by `init` after the declaration, and again when the name or the
    /// symbol changes.
    Metadata {
        /// Name, 1 to 64 bytes.
        name: String,
        /// Symbol, 1 to 16 bytes.
        symbol: String,
        /// Decimals, at most 18, never changed.
        decimals: u8,
    },
    /// `("transfer", from, to, amount)`: every change of a balance; a mint is from the zero account, a burn to it, and
    /// a fee is a transfer of its own.
    Transfer {
        /// The account debited, the zero account for a mint.
        from: [u8; 32],
        /// The account credited, the zero account for a burn.
        to: [u8; 32],
        /// Above 0.
        amount: u128,
    },
    /// `("approve", owner, spender, amount, expires_at)`: every `approve`.
    Approve {
        /// The account whose tokens the spender may take.
        owner: [u8; 32],
        /// The account allowed to take them with `transfer_from`.
        spender: [u8; 32],
        /// The allowance set (not added): 0 revokes it, [`UNLIMITED`] never goes down.
        amount: u128,
        /// The block timestamp from which the allowance no longer counts; `None` never expires.
        expires_at: Option<u64>,
    },
    /// `("minter", previous, minter)` (`KS-20-mintable`): the minter set by `init` (from `None`) or by `set_minter`.
    Minter {
        /// The minter before.
        previous: Option<[u8; 32]>,
        /// The minter now; `None` ends minting for good.
        minter: Option<[u8; 32]>,
    },
}

impl Event {
    /// Parses an event's bytes; `None` for another event or any other layout (the name and the exact fields).
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut input = data;
        let name = String::decode(&mut input)?;
        let event = match name.as_str() {
            "standards" => Self::Standards(Vec::<String>::decode(&mut input)?),
            "metadata" => {
                let (name, symbol, decimals) = <(String, String, u8)>::decode(&mut input)?;
                Self::Metadata {
                    name,
                    symbol,
                    decimals,
                }
            }
            "transfer" => {
                let (from, to, amount) = <([u8; 32], [u8; 32], u128)>::decode(&mut input)?;
                Self::Transfer { from, to, amount }
            }
            "approve" => {
                let (owner, spender, amount, expires_at) =
                    <([u8; 32], [u8; 32], u128, Option<u64>)>::decode(&mut input)?;
                Self::Approve {
                    owner,
                    spender,
                    amount,
                    expires_at,
                }
            }
            "minter" => {
                let (previous, minter) =
                    <(Option<[u8; 32]>, Option<[u8; 32]>)>::decode(&mut input)?;
                Self::Minter { previous, minter }
            }
            _ => return None,
        };
        input.is_empty().then_some(event)
    }

    /// The event's bytes.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Standards(ids) => encode(&("standards", ids)),
            Self::Metadata {
                name,
                symbol,
                decimals,
            } => encode(&("metadata", name, symbol, *decimals)),
            Self::Transfer { from, to, amount } => encode(&("transfer", from, to, *amount)),
            Self::Approve {
                owner,
                spender,
                amount,
                expires_at,
            } => encode(&("approve", owner, spender, *amount, *expires_at)),
            Self::Minter { previous, minter } => encode(&("minter", *previous, *minter)),
        }
    }

    /// Records the event.
    pub fn emit(&self) {
        crate::emit_event(&self.encode());
    }
}

/// The rules a token adds to KS-20: every method defaults to none.
pub trait Policy {
    /// Whether `amount` may move from `from` to `to`, before every change of balance: transfers (`transfer`,
    /// `transfer_from`, `transfer_and_call`), mints (`from` is the zero account) and burns (`to` is the zero account).
    /// A refusal by the token's rules should use [`crate::standard::rejected`], so that the reason starts with
    /// `rejected: `. The fee of a transfer ([`Policy::fee`]) is not checked again.
    fn check(&self, _from: &[u8; 32], _to: &[u8; 32], _amount: u128) -> Result<(), Vec<u8>> {
        Ok(())
    }

    /// The fee of a transfer of `amount` from `from` to `to`: `Some((account, fee))` takes `fee` out of the amount
    /// for `account` (the zero account burns it), so that `to` receives `amount − fee` and `from` pays `amount` in all
    /// (KS-20 rule 4). `None`, a fee of 0 or a fee for `from` itself takes nothing; a fee of `amount` or more refuses
    /// the transfer. Mints and burns pay no fee. A token whose fee can be above 0 reports [`crate::standard::CAP_FEE`]
    /// in [`Policy::capabilities`].
    fn fee(
        &self,
        _from: &[u8; 32],
        _to: &[u8; 32],
        _amount: u128,
    ) -> Result<Option<([u8; 32], u128)>, Vec<u8>> {
        Ok(None)
    }

    /// The capabilities these rules give someone now (names of [`crate::standard::CAPABILITIES`]): for example
    /// `freeze` while an admin can still freeze accounts. `mint` is reported by [`Ks20::capabilities`] itself, from the
    /// minter.
    fn capabilities(&self) -> Result<Vec<&'static str>, Vec<u8>> {
        Ok(Vec::new())
    }
}

/// No rules of its own: a plain KS-20 token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoPolicy;

impl Policy for NoPolicy {}

/// The reference implementation of KS-20, for a token contract to build on (see the module docs). It holds no state of
/// its own: a `const` of it is the token's configuration, the rules ([`Policy`]) and the extensions it implements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ks20<P = NoPolicy> {
    policy: P,
    burnable: bool,
    mintable: bool,
}

impl Ks20<NoPolicy> {
    /// A plain KS-20 token, with no extension.
    pub const fn new() -> Self {
        Self {
            policy: NoPolicy,
            burnable: false,
            mintable: false,
        }
    }

    /// The same token with the extension `KS-20-burnable` (`burn`, `burn_from`).
    pub const fn burnable(self) -> Self {
        Self {
            burnable: true,
            ..self
        }
    }

    /// The same token with the extension `KS-20-mintable` (`mint`, `minter`, `set_minter`, `cap`); its `init` calls
    /// [`Ks20::init_minter`] after [`Ks20::init`].
    pub const fn mintable(self) -> Self {
        Self {
            mintable: true,
            ..self
        }
    }

    /// The same token with the rules `policy` (after the extensions).
    pub const fn with_policy<Q: Policy>(self, policy: Q) -> Ks20<Q> {
        Ks20 {
            policy,
            burnable: self.burnable,
            mintable: self.mintable,
        }
    }
}

/// Reads an amount; 0 when absent.
fn read_amount(key: &[u8]) -> Result<u128, Vec<u8>> {
    Ok(storage::read::<u128>(key)?.unwrap_or(0))
}

/// Writes an amount; 0 removes the key, so that each state has exactly one representation.
fn write_amount(key: &[u8], amount: u128) {
    if amount == 0 {
        storage::remove(key);
    } else {
        storage::write(key, &amount);
    }
}

fn balance_key(account: &[u8; 32]) -> Vec<u8> {
    [PREFIX_BALANCE, account.as_slice()].concat()
}

fn allowance_key(owner: &[u8; 32], spender: &[u8; 32]) -> Vec<u8> {
    [PREFIX_ALLOWANCE, owner.as_slice(), spender.as_slice()].concat()
}

/// The arguments of `approve`: `(spender, amount, expires_at)`, or `(spender, amount)` with no expiry, the form the
/// token example took before KS-20 (callers of KS-20 send all three).
struct Approval([u8; 32], u128, Option<u64>);

impl Decode for Approval {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        let (spender, amount) = <([u8; 32], u128)>::decode(input)?;
        let expires_at = if input.is_empty() {
            None
        } else {
            Option::<u64>::decode(input)?
        };
        Some(Self(spender, amount, expires_at))
    }
}

impl<P: Policy> Ks20<P> {
    /// The token's rules.
    pub fn policy(&self) -> &P {
        &self.policy
    }

    /// The standards the token declares: `KS-20`, then its extensions.
    pub fn standards(&self) -> Vec<String> {
        let mut ids = Vec::from([String::from(ID)]);
        if self.burnable {
            ids.push(String::from(BURNABLE));
        }
        if self.mintable {
            ids.push(String::from(MINTABLE));
        }
        ids
    }

    /// The token's `init`: checks and records the metadata and emits the declaration and the metadata, before any
    /// transfer (KS-20 rule 1). Mint the initial supply with [`Ks20::mint`] after it.
    pub fn init(&self, name: &str, symbol: &str, decimals: u8) -> Result<(), Vec<u8>> {
        if storage::get(KEY_METADATA).is_some() {
            return Err(ERR_ALREADY_INITIALIZED.to_vec());
        }
        check_name(name)?;
        check_symbol(symbol)?;
        if decimals > MAX_DECIMALS {
            return Err(ERR_DECIMALS.to_vec());
        }
        storage::write(KEY_METADATA, &(name, symbol, decimals));
        Event::Standards(self.standards()).emit();
        Event::Metadata {
            name: String::from(name),
            symbol: String::from(symbol),
            decimals,
        }
        .emit();
        Ok(())
    }

    /// `KS-20-mintable`: sets the first minter (`None` mints nothing ever) and the cap on the total supply (`None` for
    /// no cap), in the token's `init` after [`Ks20::init`]; emits `minter` from `None`.
    pub fn init_minter(&self, minter: Option<[u8; 32]>, cap: Option<u128>) -> Result<(), Vec<u8>> {
        if storage::get(KEY_MINTER).is_some() {
            return Err(ERR_ALREADY_INITIALIZED.to_vec());
        }
        if minter == Some(ZERO_ACCOUNT) {
            return Err(ERR_ZERO_ACCOUNT.to_vec());
        }
        if let Some(cap) = cap {
            if cap < self.total_supply()? {
                return Err(ERR_CAP_EXCEEDED.to_vec());
            }
            storage::write(KEY_CAP, &cap);
        }
        storage::write(KEY_MINTER, &minter);
        Event::Minter {
            previous: None,
            minter,
        }
        .emit();
        Ok(())
    }

    /// Answers the standard's methods (and those of the extensions the token has); `None` for any other method, `init`
    /// included, which the token's own dispatch function then answers.
    pub fn dispatch(&self, method: &str, args: &[u8]) -> Option<CallResult> {
        let result = match method {
            STANDARDS => run(args, |()| Ok(self.standards())),
            METADATA => run(args, |()| self.metadata()),
            CAPABILITIES_VIEW => run(args, |()| self.capabilities()),
            "total_supply" => run(args, |()| self.total_supply()),
            "balance_of" => run(args, |account: [u8; 32]| self.balance_of(&account)),
            "allowance" => run(args, |(owner, spender): ([u8; 32], [u8; 32])| {
                self.allowance(&owner, &spender)
            }),
            "allowance_info" => run(args, |(owner, spender): ([u8; 32], [u8; 32])| {
                self.allowance_info(&owner, &spender)
            }),
            "transfer" => run(args, |(to, amount): ([u8; 32], u128)| {
                self.transfer(&crate::caller(), &to, amount).map(|_| ())
            }),
            "transfer_from" => run(args, |(from, to, amount): ([u8; 32], [u8; 32], u128)| {
                self.transfer_from(&crate::caller(), &from, &to, amount)
                    .map(|_| ())
            }),
            "approve" => run(args, |raw: Approval| {
                self.approve(&crate::caller(), &raw.0, raw.1, raw.2)
            }),
            // the one payable method: the funds go on to the recipient
            "transfer_and_call" => self.dispatch_transfer_and_call(args),
            "burn" if self.burnable => {
                run(args, |amount: u128| self.burn(&crate::caller(), amount))
            }
            "burn_from" if self.burnable => run(args, |(from, amount): ([u8; 32], u128)| {
                self.burn_from(&crate::caller(), &from, amount)
            }),
            "mint" if self.mintable => run(args, |(to, amount): ([u8; 32], u128)| {
                self.only_minter()?;
                self.mint(&to, amount)
            }),
            "minter" if self.mintable => run(args, |()| self.minter()),
            "set_minter" if self.mintable => run(args, |minter: Option<[u8; 32]>| {
                self.only_minter()?;
                self.set_minter(minter)
            }),
            "cap" if self.mintable => run(args, |()| self.cap()),
            _ => return None,
        };
        Some(result)
    }

    /// `metadata() -> (name, symbol, decimals)`.
    pub fn metadata(&self) -> Result<(String, String, u8), Vec<u8>> {
        storage::read(KEY_METADATA)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
    }

    /// Changes the name and the symbol (never the decimals) and emits `metadata` again: wallets flag the change.
    pub fn set_metadata(&self, name: &str, symbol: &str) -> Result<(), Vec<u8>> {
        check_name(name)?;
        check_symbol(symbol)?;
        let (_, _, decimals) = self.metadata()?;
        storage::write(KEY_METADATA, &(name, symbol, decimals));
        Event::Metadata {
            name: String::from(name),
            symbol: String::from(symbol),
            decimals,
        }
        .emit();
        Ok(())
    }

    /// `capabilities()`: `mint` while a minter is set, and what the [`Policy`] reports, in the order of
    /// [`crate::standard::CAPABILITIES`] (names it does not know last).
    pub fn capabilities(&self) -> Result<Vec<String>, Vec<u8>> {
        let mut present = self.policy.capabilities()?;
        if self.mintable && self.minter()?.is_some() {
            present.push(CAP_MINT);
        }
        Ok(ordered(&present))
    }

    /// `total_supply()`: every balance added up.
    pub fn total_supply(&self) -> Result<u128, Vec<u8>> {
        read_amount(KEY_SUPPLY)
    }

    /// `balance_of(account)`.
    pub fn balance_of(&self, account: &[u8; 32]) -> Result<u128, Vec<u8>> {
        read_amount(&balance_key(account))
    }

    /// `allowance_info(owner, spender) -> (amount, expires_at)` as set, expired or not; `(0, None)` when there is none.
    pub fn allowance_info(
        &self,
        owner: &[u8; 32],
        spender: &[u8; 32],
    ) -> Result<(u128, Option<u64>), Vec<u8>> {
        Ok(storage::read(&allowance_key(owner, spender))?.unwrap_or((0, None)))
    }

    /// `allowance(owner, spender)`: what `spender` may take from `owner` now, 0 once the allowance expired.
    pub fn allowance(&self, owner: &[u8; 32], spender: &[u8; 32]) -> Result<u128, Vec<u8>> {
        let (amount, expires_at) = self.allowance_info(owner, spender)?;
        Ok(if expired(expires_at) { 0 } else { amount })
    }

    /// `minter()` (`KS-20-mintable`): `None` once minting ended, or when the token was never mintable.
    pub fn minter(&self) -> Result<Option<[u8; 32]>, Vec<u8>> {
        Ok(storage::read::<Option<[u8; 32]>>(KEY_MINTER)?.flatten())
    }

    /// `cap()` (`KS-20-mintable`): the most the total supply can reach; `None` for no cap.
    pub fn cap(&self) -> Result<Option<u128>, Vec<u8>> {
        storage::read(KEY_CAP)
    }

    /// Reverts unless the caller is the minter.
    fn only_minter(&self) -> Result<(), Vec<u8>> {
        match self.minter()? {
            None => Err(ERR_MINTING_ENDED.to_vec()),
            Some(minter) if minter == crate::caller() => Ok(()),
            Some(_) => Err(ERR_NOT_THE_MINTER.to_vec()),
        }
    }

    /// Sets the minter (`KS-20-mintable`; the method checks the caller first): `None` ends minting for good. Emits
    /// `minter`.
    pub fn set_minter(&self, minter: Option<[u8; 32]>) -> Result<(), Vec<u8>> {
        if minter == Some(ZERO_ACCOUNT) {
            return Err(ERR_ZERO_ACCOUNT.to_vec());
        }
        let previous = self.minter()?;
        if previous.is_none() {
            return Err(ERR_MINTING_ENDED.to_vec());
        }
        storage::write(KEY_MINTER, &minter);
        Event::Minter { previous, minter }.emit();
        Ok(())
    }

    fn credit(&self, account: &[u8; 32], amount: u128) -> Result<(), Vec<u8>> {
        let key = balance_key(account);
        let balance = read_amount(&key)?
            .checked_add(amount)
            .ok_or_else(|| ERR_OVERFLOW.to_vec())?;
        write_amount(&key, balance);
        Ok(())
    }

    fn debit(&self, account: &[u8; 32], amount: u128) -> Result<(), Vec<u8>> {
        let key = balance_key(account);
        let balance = read_amount(&key)?
            .checked_sub(amount)
            .ok_or_else(|| ERR_INSUFFICIENT_BALANCE.to_vec())?;
        write_amount(&key, balance);
        Ok(())
    }

    /// Moves `amount` from `from` to `to` once the arguments are checked: the [`Policy`]'s check and fee, the balances,
    /// the events; returns what `to` received.
    fn send(&self, from: &[u8; 32], to: &[u8; 32], amount: u128) -> Result<u128, Vec<u8>> {
        self.policy.check(from, to, amount)?;
        let fee = match self.policy.fee(from, to, amount)? {
            Some((account, fee)) if fee > 0 && account != *from => Some((account, fee)),
            _ => None,
        };
        let net = match fee {
            Some((_, fee)) => amount
                .checked_sub(fee)
                .filter(|net| *net > 0)
                .ok_or_else(|| rejected(REASON_FEE))?,
            None => amount,
        };
        // `from` is debited before `to` is read, so the result is right when `from == to`
        self.debit(from, amount)?;
        self.credit(to, net)?;
        Event::Transfer {
            from: *from,
            to: *to,
            amount: net,
        }
        .emit();
        if let Some((account, fee)) = fee {
            if account == ZERO_ACCOUNT {
                let supply = self
                    .total_supply()?
                    .checked_sub(fee)
                    .ok_or_else(|| ERR_OVERFLOW.to_vec())?;
                write_amount(KEY_SUPPLY, supply);
            } else {
                self.credit(&account, fee)?;
            }
            Event::Transfer {
                from: *from,
                to: account,
                amount: fee,
            }
            .emit();
        }
        Ok(net)
    }

    /// `transfer`: moves `amount` from `from` to `to` by the token's rules; returns what `to` received.
    pub fn transfer(&self, from: &[u8; 32], to: &[u8; 32], amount: u128) -> Result<u128, Vec<u8>> {
        if amount == 0 {
            return Err(ERR_ZERO_AMOUNT.to_vec());
        }
        check_recipient(to)?;
        self.send(from, to, amount)
    }

    /// `transfer_from` by `spender`: takes `amount` of `from`'s allowance to `spender` (none needed when `spender` is
    /// `from`) and moves it to `to`; returns what `to` received.
    pub fn transfer_from(
        &self,
        spender: &[u8; 32],
        from: &[u8; 32],
        to: &[u8; 32],
        amount: u128,
    ) -> Result<u128, Vec<u8>> {
        if amount == 0 {
            return Err(ERR_ZERO_AMOUNT.to_vec());
        }
        check_recipient(to)?;
        if spender != from {
            self.spend_allowance(from, spender, amount)?;
        }
        self.send(from, to, amount)
    }

    /// `transfer_and_call`: moves `amount` from `from` to the contract `to`, then calls `to`'s
    /// `on_token_received(from, received, data)` passing on the funds attached to this call, and returns the content of
    /// the `Bytes` it answered. When `to` is no contract, refuses or answers anything but `Bytes`, the whole op fails and
    /// nothing moves.
    pub fn transfer_and_call(
        &self,
        from: &[u8; 32],
        to: &[u8; 32],
        amount: u128,
        data: &[u8],
    ) -> Result<Vec<u8>, Vec<u8>> {
        if amount == 0 {
            return Err(ERR_ZERO_AMOUNT.to_vec());
        }
        check_recipient(to)?;
        // checks and effects first, then the call (the recipient may call this token back)
        let net = self.send(from, to, amount)?;
        let funds = crate::call_funds();
        let answer = crate::call_with_funds(to, ON_RECEIVED, &encode(&(*from, net, data)), &funds);
        decode_return(&answer)
    }

    /// The method `transfer_and_call`: its answer is the recipient's, the same `Bytes`.
    fn dispatch_transfer_and_call(&self, args: &[u8]) -> CallResult {
        let (to, amount, data): ([u8; 32], u128, Vec<u8>) = decode(args)?;
        let content = self.transfer_and_call(&crate::caller(), &to, amount, &data)?;
        crate::ok(&content)
    }

    /// `approve` by `owner`: sets `spender`'s allowance (not added to it); 0 revokes it, and then `expires_at` is
    /// dropped. Emits `approve`.
    pub fn approve(
        &self,
        owner: &[u8; 32],
        spender: &[u8; 32],
        amount: u128,
        expires_at: Option<u64>,
    ) -> Result<(), Vec<u8>> {
        if *spender == ZERO_ACCOUNT {
            return Err(ERR_ZERO_ACCOUNT.to_vec());
        }
        if spender == owner {
            return Err(ERR_SPENDER_IS_CALLER.to_vec());
        }
        let key = allowance_key(owner, spender);
        let expires_at = if amount == 0 { None } else { expires_at };
        if amount == 0 {
            storage::remove(&key);
        } else {
            storage::write(&key, &(amount, expires_at));
        }
        Event::Approve {
            owner: *owner,
            spender: *spender,
            amount,
            expires_at,
        }
        .emit();
        Ok(())
    }

    /// Takes `amount` of `owner`'s allowance to `spender`: reverts when it expired or is short; [`UNLIMITED`] stays.
    pub fn spend_allowance(
        &self,
        owner: &[u8; 32],
        spender: &[u8; 32],
        amount: u128,
    ) -> Result<(), Vec<u8>> {
        let key = allowance_key(owner, spender);
        let (allowed, expires_at) = storage::read(&key)?.unwrap_or((0u128, None::<u64>));
        if allowed > 0 && expired(expires_at) {
            return Err(ERR_ALLOWANCE_EXPIRED.to_vec());
        }
        if allowed == UNLIMITED {
            return Ok(());
        }
        let left = allowed
            .checked_sub(amount)
            .ok_or_else(|| ERR_INSUFFICIENT_ALLOWANCE.to_vec())?;
        if left == 0 {
            storage::remove(&key);
        } else {
            storage::write(&key, &(left, expires_at));
        }
        Ok(())
    }

    /// Creates `amount` tokens for `to` (the [`Policy`] checks it as a movement from the zero account); past the cap,
    /// if there is one, it reverts. `mint` of `KS-20-mintable` checks that the caller is the minter first; a token's
    /// `init` mints its initial supply with it.
    pub fn mint(&self, to: &[u8; 32], amount: u128) -> Result<(), Vec<u8>> {
        if amount == 0 {
            return Err(ERR_ZERO_AMOUNT.to_vec());
        }
        check_recipient(to)?;
        self.policy.check(&ZERO_ACCOUNT, to, amount)?;
        let supply = self
            .total_supply()?
            .checked_add(amount)
            .ok_or_else(|| ERR_SUPPLY_OVERFLOW.to_vec())?;
        if self.cap()?.is_some_and(|cap| supply > cap) {
            return Err(ERR_CAP_EXCEEDED.to_vec());
        }
        write_amount(KEY_SUPPLY, supply);
        self.credit(to, amount)?;
        Event::Transfer {
            from: ZERO_ACCOUNT,
            to: *to,
            amount,
        }
        .emit();
        Ok(())
    }

    /// Destroys `amount` of `from`'s tokens (the [`Policy`] checks it as a movement to the zero account).
    pub fn burn(&self, from: &[u8; 32], amount: u128) -> Result<(), Vec<u8>> {
        if amount == 0 {
            return Err(ERR_ZERO_AMOUNT.to_vec());
        }
        self.policy.check(from, &ZERO_ACCOUNT, amount)?;
        self.debit(from, amount)?;
        let supply = self
            .total_supply()?
            .checked_sub(amount)
            .ok_or_else(|| ERR_OVERFLOW.to_vec())?;
        write_amount(KEY_SUPPLY, supply);
        Event::Transfer {
            from: *from,
            to: ZERO_ACCOUNT,
            amount,
        }
        .emit();
        Ok(())
    }

    /// `burn_from` by `spender` (`KS-20-burnable`): takes `amount` of `from`'s allowance to `spender` (none needed when
    /// `spender` is `from`) and destroys it.
    pub fn burn_from(
        &self,
        spender: &[u8; 32],
        from: &[u8; 32],
        amount: u128,
    ) -> Result<(), Vec<u8>> {
        if amount == 0 {
            return Err(ERR_ZERO_AMOUNT.to_vec());
        }
        if spender != from {
            self.spend_allowance(from, spender, amount)?;
        }
        self.burn(from, amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{self, Ks20Subject, ALICE, BOB};
    use crate::mock::{self, Failure};
    use crate::standard::{is_rejection, CAP_FEE, CAP_FREEZE, ERR_NAME, ERR_UNKNOWN_METHOD};
    use alloc::vec;

    const HOLDER: [u8; 32] = [0x11; 32];
    const COLLECTOR: [u8; 32] = [0x22; 32];
    const FROZEN: [u8; 32] = [0x33; 32];

    /// The rules of the fee token: 10% of every transfer to `COLLECTOR` (none below 10 base units), and `FROZEN` can
    /// neither send nor receive.
    struct Rules;

    impl Policy for Rules {
        fn check(&self, from: &[u8; 32], to: &[u8; 32], _amount: u128) -> Result<(), Vec<u8>> {
            if *from == FROZEN || *to == FROZEN {
                return Err(rejected("account is frozen"));
            }
            Ok(())
        }

        fn fee(
            &self,
            _from: &[u8; 32],
            _to: &[u8; 32],
            amount: u128,
        ) -> Result<Option<([u8; 32], u128)>, Vec<u8>> {
            Ok(Some((COLLECTOR, amount / 10)))
        }

        fn capabilities(&self) -> Result<Vec<&'static str>, Vec<u8>> {
            Ok(vec![CAP_FEE, CAP_FREEZE])
        }
    }

    const PLAIN: Ks20 = Ks20::new().burnable().mintable();
    const FEES: Ks20<Rules> = Ks20::new().with_policy(Rules);

    /// `init(name, supply, minter, cap)`.
    fn plain(method: &str, args: &[u8]) -> CallResult {
        if method == "init" {
            let (name, supply, minter, cap): (String, u128, Option<[u8; 32]>, Option<u128>) =
                decode(args)?;
            PLAIN.init(&name, "PLN", 8)?;
            PLAIN.init_minter(minter, cap)?;
            PLAIN.mint(&crate::caller(), supply)?;
            return crate::ok(&());
        }
        PLAIN
            .dispatch(method, args)
            .unwrap_or_else(|| Err(ERR_UNKNOWN_METHOD.to_vec()))
    }

    fn fees(method: &str, args: &[u8]) -> CallResult {
        if method == "init" {
            FEES.init("Fee token", "FEE", 0)?;
            FEES.mint(&crate::caller(), decode(args)?)?;
            return crate::ok(&());
        }
        FEES.dispatch(method, args)
            .unwrap_or_else(|| Err(ERR_UNKNOWN_METHOD.to_vec()))
    }

    fn deploy_plain(minter: Option<[u8; 32]>, cap: Option<u128>) -> Result<[u8; 32], Failure> {
        let init = encode(&("Plain ₿", 1_000_000u128, minter, cap));
        mock::deploy(HOLDER, 0, b"plain", plain, &init)
    }

    fn deploy_fees() -> [u8; 32] {
        mock::deploy(HOLDER, 0, b"fees", fees, &encode(&1_000_000u128)).unwrap()
    }

    fn balance(token: &[u8; 32], account: &[u8; 32]) -> u128 {
        decode(&mock::call(BOB, *token, "balance_of", &encode(account)).unwrap()).unwrap()
    }

    fn token_events(token: &[u8; 32]) -> Vec<Event> {
        mock::events()
            .into_iter()
            .filter(|e| e.contract == *token)
            .filter_map(|e| Event::parse(&e.data))
            .collect()
    }

    #[test]
    fn the_reference_token_conforms() {
        for minter in [Some(HOLDER), None] {
            conformance::ks20(|| Ks20Subject {
                token: deploy_plain(minter, Some(2_000_000)).unwrap(),
                holder: HOLDER,
            })
            .unwrap();
        }
        conformance::ks20(|| Ks20Subject {
            token: deploy_fees(),
            holder: HOLDER,
        })
        .unwrap();
    }

    #[test]
    fn fees_come_out_of_the_amount() {
        mock::reset();
        let token = deploy_fees();
        mock::clear_events();
        mock::call(HOLDER, token, "transfer", &encode(&(ALICE, 100u128))).unwrap();
        assert_eq!(balance(&token, &HOLDER), 1_000_000 - 100);
        assert_eq!(balance(&token, &ALICE), 90);
        assert_eq!(balance(&token, &COLLECTOR), 10);
        assert_eq!(
            token_events(&token),
            [
                Event::Transfer {
                    from: HOLDER,
                    to: ALICE,
                    amount: 90
                },
                Event::Transfer {
                    from: HOLDER,
                    to: COLLECTOR,
                    amount: 10
                }
            ]
        );
        // below 10 base units the fee is 0
        mock::call(HOLDER, token, "transfer", &encode(&(ALICE, 9u128))).unwrap();
        assert_eq!(balance(&token, &ALICE), 99);
        let capabilities: Vec<String> =
            decode(&mock::call(BOB, token, "capabilities", &[]).unwrap()).unwrap();
        assert_eq!(capabilities, [String::from("freeze"), String::from("fee")]);
    }

    #[test]
    fn refusals_by_the_rules_carry_the_prefix() {
        mock::reset();
        let token = deploy_fees();
        let Err(Failure::Revert { reason, .. }) =
            mock::call(HOLDER, token, "transfer", &encode(&(FROZEN, 5u128)))
        else {
            panic!("the transfer to a frozen account went through");
        };
        assert_eq!(reason, b"rejected: account is frozen");
        assert!(is_rejection(&reason));
    }

    #[test]
    fn a_fee_of_the_whole_amount_is_refused() {
        struct Greedy;
        impl Policy for Greedy {
            fn fee(
                &self,
                _from: &[u8; 32],
                _to: &[u8; 32],
                amount: u128,
            ) -> Result<Option<([u8; 32], u128)>, Vec<u8>> {
                Ok(Some((COLLECTOR, amount)))
            }
        }
        const GREEDY: Ks20<Greedy> = Ks20::new().with_policy(Greedy);
        mock::reset();
        mock::with_context(HOLDER, [0x44; 32], || {
            GREEDY.init("Greedy", "GRD", 0).unwrap();
            GREEDY.mint(&HOLDER, 10).unwrap();
            assert_eq!(
                GREEDY.transfer(&HOLDER, &ALICE, 10),
                Err(b"rejected: the fee takes the whole amount".to_vec())
            );
            // a fee for the sender itself takes nothing
            struct Own;
            impl Policy for Own {
                fn fee(
                    &self,
                    from: &[u8; 32],
                    _to: &[u8; 32],
                    amount: u128,
                ) -> Result<Option<([u8; 32], u128)>, Vec<u8>> {
                    Ok(Some((*from, amount)))
                }
            }
            assert_eq!(
                Ks20::new().with_policy(Own).transfer(&HOLDER, &ALICE, 4),
                Ok(4)
            );
        });
    }

    #[test]
    fn a_transfer_to_oneself_keeps_the_balance() {
        mock::reset();
        let token = deploy_plain(None, None).unwrap();
        mock::call(HOLDER, token, "transfer", &encode(&(HOLDER, 5u128))).unwrap();
        assert_eq!(balance(&token, &HOLDER), 1_000_000);
        let token = deploy_fees_at(1);
        mock::call(HOLDER, token, "transfer", &encode(&(HOLDER, 100u128))).unwrap();
        assert_eq!(balance(&token, &HOLDER), 1_000_000 - 10);
        assert_eq!(balance(&token, &COLLECTOR), 10);
    }

    fn deploy_fees_at(nonce: u64) -> [u8; 32] {
        mock::deploy(HOLDER, nonce, b"fees", fees, &encode(&1_000_000u128)).unwrap()
    }

    #[test]
    fn the_two_argument_approve_has_no_expiry() {
        mock::reset();
        let token = deploy_plain(None, None).unwrap();
        mock::call(HOLDER, token, "approve", &encode(&(ALICE, 7u128))).unwrap();
        let info: (u128, Option<u64>) =
            decode(&mock::call(BOB, token, "allowance_info", &encode(&(HOLDER, ALICE))).unwrap())
                .unwrap();
        assert_eq!(info, (7, None));
        let event = token_events(&token).pop().unwrap();
        assert_eq!(
            event,
            Event::Approve {
                owner: HOLDER,
                spender: ALICE,
                amount: 7,
                expires_at: None
            }
        );
        // the expiry, when given, is an Option
        let wrong = encode(&(ALICE, 7u128, 9u64));
        assert!(mock::call(HOLDER, token, "approve", &wrong).is_err());
    }

    #[test]
    fn minting_stops_at_the_cap_and_for_good() {
        mock::reset();
        assert!(deploy_plain(Some(HOLDER), Some(999_999)).is_err());
        let token = deploy_plain(Some(HOLDER), Some(1_000_010)).unwrap();
        let mint = |caller: [u8; 32], amount: u128| {
            mock::call(caller, token, "mint", &encode(&(ALICE, amount)))
        };
        mint(HOLDER, 10).unwrap();
        let Err(Failure::Revert { reason, .. }) = mint(HOLDER, 1) else {
            panic!("a mint past the cap");
        };
        assert_eq!(reason, ERR_CAP_EXCEEDED);
        // the role moves, then ends
        mock::call(HOLDER, token, "set_minter", &encode(&Some(BOB))).unwrap();
        assert!(mint(HOLDER, 0).is_err());
        let Err(Failure::Revert { reason, .. }) =
            mock::call(HOLDER, token, "set_minter", &encode(&None::<[u8; 32]>))
        else {
            panic!("set_minter by the former minter");
        };
        assert_eq!(reason, ERR_NOT_THE_MINTER);
        mock::call(BOB, token, "set_minter", &encode(&None::<[u8; 32]>)).unwrap();
        let Err(Failure::Revert { reason, .. }) =
            mock::call(BOB, token, "set_minter", &encode(&Some(BOB)))
        else {
            panic!("set_minter after minting ended");
        };
        assert_eq!(reason, ERR_MINTING_ENDED);
        let capabilities: Vec<String> =
            decode(&mock::call(BOB, token, "capabilities", &[]).unwrap()).unwrap();
        assert!(capabilities.is_empty());
        let minters: Vec<Event> = token_events(&token)
            .into_iter()
            .filter(|e| matches!(e, Event::Minter { .. }))
            .collect();
        assert_eq!(
            minters,
            [
                Event::Minter {
                    previous: None,
                    minter: Some(HOLDER)
                },
                Event::Minter {
                    previous: Some(HOLDER),
                    minter: Some(BOB)
                },
                Event::Minter {
                    previous: Some(BOB),
                    minter: None
                }
            ]
        );
    }

    #[test]
    fn init_checks_the_metadata() {
        mock::reset();
        let init = encode(&("", 1u128, None::<[u8; 32]>, None::<u128>));
        let Err(Failure::Revert { reason, .. }) = mock::deploy(HOLDER, 0, b"plain", plain, &init)
        else {
            panic!("an empty name");
        };
        assert_eq!(reason, ERR_NAME);
        mock::with_context(HOLDER, [0x44; 32], || {
            assert_eq!(Ks20::new().init("T", "T", 19), Err(ERR_DECIMALS.to_vec()));
            Ks20::new().init("T", "T", 18).unwrap();
            assert_eq!(
                Ks20::new().init("T", "T", 18),
                Err(ERR_ALREADY_INITIALIZED.to_vec())
            );
        });
    }

    #[test]
    fn events_round_trip() {
        let events = [
            Event::Standards(vec![String::from(ID), String::from(BURNABLE)]),
            Event::Metadata {
                name: String::from("Token ₿"),
                symbol: String::from("TKN"),
                decimals: 8,
            },
            Event::Transfer {
                from: [1; 32],
                to: [2; 32],
                amount: 3,
            },
            Event::Approve {
                owner: [1; 32],
                spender: [2; 32],
                amount: 3,
                expires_at: Some(4),
            },
            Event::Minter {
                previous: None,
                minter: Some([5; 32]),
            },
        ];
        for event in events {
            assert_eq!(Event::parse(&event.encode()), Some(event.clone()));
            let mut longer = event.encode();
            longer.push(0);
            assert_eq!(Event::parse(&longer), None, "{event:?}");
        }
        // the layout is the one the existing token example uses for transfers
        let transfer = Event::Transfer {
            from: [1; 32],
            to: [2; 32],
            amount: 3,
        };
        assert_eq!(
            transfer.encode(),
            borsh::to_vec(&(String::from("transfer"), [1u8; 32], [2u8; 32], 3u128)).unwrap()
        );
        assert_eq!(transfer.encode().len(), 92);
        // the token example's approve, without the expiry, is not a KS-20 approve
        let old = borsh::to_vec(&(String::from("approve"), [1u8; 32], [2u8; 32], 3u128)).unwrap();
        assert_eq!(Event::parse(&old), None);
        assert_eq!(Event::parse(b"\x03\x00\x00\x00abc"), None);
    }

    #[test]
    fn standards_list_the_extensions() {
        assert_eq!(Ks20::new().standards(), vec![String::from(ID)]);
        assert_eq!(
            Ks20::new().mintable().burnable().standards(),
            vec![
                String::from(ID),
                String::from(BURNABLE),
                String::from(MINTABLE)
            ]
        );
    }
}
