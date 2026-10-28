//! KS-721, the non-fungible token standard (`docs/STANDARDS.md`, D-120).
//!
//! - [`Collection`]: calling a KS-721 collection from another contract;
//! - [`received`]: what `transfer_and_call` gives a contract's `on_nft_received`;
//! - [`Event`]: the standard's events, parsed and emitted;
//! - [`id`] / [`serial`]: token IDs from serial numbers and back;
//! - [`Ks721`]: the reference implementation a collection is built on (ownership, approvals, operators with an
//!   optional expiry, `transfer_and_call`, token URIs, the declaration, the extensions `KS-721-burnable` and
//!   `KS-721-enumerable`), with the collection's own rules in a [`Policy`].
//!
//! # A collection
//!
//! ```ignore
//! #![cfg_attr(target_arch = "wasm32", no_std)]
//! extern crate alloc;
//!
//! use alloc::string::String;
//! use ksana_contract_sdk::{self as sdk, ks721::{self, Ks721}, standard, storage, CallResult};
//!
//! sdk::entry!(dispatch);
//!
//! const NFT: Ks721 = Ks721::new().burnable().enumerable();
//!
//! pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
//!     match method {
//!         "init" => {
//!             standard::no_funds()?;
//!             let (name, symbol): (String, String) = sdk::decode(args)?;
//!             NFT.init(&name, &symbol)?;
//!             storage::write(b"minter", &sdk::caller());
//!             sdk::ok(&())
//!         }
//!         "mint" => {
//!             standard::no_funds()?;
//!             let (to, serial, uri): ([u8; 32], u128, Option<String>) = sdk::decode(args)?;
//!             if storage::read::<[u8; 32]>(b"minter")? != Some(sdk::caller()) {
//!                 return sdk::revert("not the minter");
//!             }
//!             NFT.mint(&to, &ks721::id(serial), uri.as_deref())?;
//!             sdk::ok(&())
//!         }
//!         _ => NFT.dispatch(method, args).unwrap_or_else(|| sdk::revert("unknown method")),
//!     }
//! }
//! ```
//!
//! Minting is the collection's own method: collections number their tokens in different ways (serial numbers, the hash
//! of a name, a BTC transaction ID), so the standard leaves the method out and [`Ks721::mint`] takes the ID. A
//! collection that can still mint reports `mint` in [`Policy::capabilities`]. A method the collection answers itself
//! before calling [`Ks721::dispatch`] replaces the implementation's (a `token_uri` computed from the ID, for example).
//!
//! # Storage
//!
//! The implementation keeps its state under keys that start with `KS721:`; a collection built on it keeps its own
//! state under other keys.

use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::{decode, encode, CallResult, Decode};
use crate::standard::{
    check_name, check_recipient, check_symbol, decode_return, expired, ordered, run,
    CAPABILITIES_VIEW, ERR_ALREADY_INITIALIZED, ERR_MISSING_INDEX, ERR_NOT_INITIALIZED,
    ERR_OVERFLOW, ERR_ZERO_ACCOUNT, METADATA, STANDARDS, ZERO_ACCOUNT,
};
use crate::{storage, CallError, Funds};

/// The standard's ID, the first name `standards()` lists.
pub const ID: &str = "KS-721";
/// The extension of `burn`.
pub const BURNABLE: &str = "KS-721-burnable";
/// The extension of `total_supply`, `tokens` and `tokens_of`.
pub const ENUMERABLE: &str = "KS-721-enumerable";
/// The method `transfer_and_call` calls on the recipient.
pub const ON_RECEIVED: &str = "on_nft_received";
/// The longest token URI the reference implementation stores, in bytes.
pub const MAX_URI_BYTES: usize = 2_048;
/// The most token IDs a page of `tokens` or `tokens_of` holds (a larger `limit` is cut to it).
pub const MAX_PAGE: u32 = 100;

/// A token ID: 32 bytes (a serial number in the last 16, big-endian, or a hash).
pub type TokenId = [u8; 32];

/// No token has this ID (never minted, or burned).
pub const ERR_UNKNOWN_TOKEN: &[u8] = b"unknown token";
/// `transfer` or `transfer_and_call` by an account that does not own the token.
pub const ERR_NOT_THE_OWNER: &[u8] = b"not the owner";
/// The caller is neither the owner, nor the token's approved account, nor an operator of the owner (or, for
/// `approve`, neither the owner nor an operator).
pub const ERR_NOT_AUTHORIZED: &[u8] = b"not authorized";
/// `transfer_from` naming an account that does not own the token now.
pub const ERR_WRONG_OWNER: &[u8] = b"wrong owner";
/// A transfer to the account that owns the token.
pub const ERR_ALREADY_THE_OWNER: &[u8] = b"already the owner";
/// `approve` of the owner itself.
pub const ERR_SPENDER_IS_OWNER: &[u8] = b"spender is the owner";
/// `set_operator` of the caller itself.
pub const ERR_OPERATOR_IS_CALLER: &[u8] = b"operator is the caller";
/// A mint of an ID a token has now.
pub const ERR_TOKEN_EXISTS: &[u8] = b"token exists";
/// A mint of an ID whose token was burned: burned IDs are never minted again.
pub const ERR_TOKEN_BURNED: &[u8] = b"token was burned";
/// A token URI over [`MAX_URI_BYTES`].
pub const ERR_URI_TOO_LONG: &[u8] = b"uri too long";

const KEY_METADATA: &[u8] = b"KS721:m";
const KEY_SUPPLY: &[u8] = b"KS721:s";
const PREFIX_OWNER: &[u8] = b"KS721:w";
const PREFIX_BALANCE: &[u8] = b"KS721:b";
const PREFIX_APPROVED: &[u8] = b"KS721:a";
const PREFIX_OPERATOR: &[u8] = b"KS721:o";
const PREFIX_URI: &[u8] = b"KS721:u";
const PREFIX_BURNED: &[u8] = b"KS721:x";
const PREFIX_ALL: &[u8] = b"KS721:T";
const PREFIX_ALL_POSITION: &[u8] = b"KS721:t";
const PREFIX_OWNED: &[u8] = b"KS721:L";
const PREFIX_OWNED_POSITION: &[u8] = b"KS721:l";

/// The token ID of serial number `n`: `n` big-endian in the last 16 bytes, so that IDs sort as their numbers do.
pub fn id(n: u128) -> TokenId {
    let mut out = [0u8; 32];
    for (byte, value) in out.iter_mut().skip(16).zip(n.to_be_bytes()) {
        *byte = value;
    }
    out
}

/// The serial number of a token ID whose first 16 bytes are zero ([`id`]); `None` for any other ID (a hash), which a
/// wallet shows in hex.
pub fn serial(token_id: &TokenId) -> Option<u128> {
    let (high, low) = token_id.split_at(16);
    if high.iter().any(|b| *b != 0) {
        return None;
    }
    Some(u128::from_be_bytes(low.try_into().ok()?))
}

/// A KS-721 collection, called from a contract. The calls that move tokens fail the whole op when the collection
/// refuses; the `try_` ones return the refusal instead ([`crate::try_call`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collection(pub [u8; 32]);

impl Collection {
    fn view<T: Decode>(&self, method: &str, args: &[u8]) -> Result<T, Vec<u8>> {
        decode_return(&crate::call(&self.0, method, args))
    }

    /// `standards()`.
    pub fn standards(&self) -> Result<Vec<String>, Vec<u8>> {
        self.view(STANDARDS, &[])
    }

    /// `metadata() -> (name, symbol)`.
    pub fn metadata(&self) -> Result<(String, String), Vec<u8>> {
        self.view(METADATA, &[])
    }

    /// `owner_of(token_id)`: `None` for a token that does not exist.
    pub fn owner_of(&self, token_id: &TokenId) -> Result<Option<[u8; 32]>, Vec<u8>> {
        self.view("owner_of", &encode(token_id))
    }

    /// `balance_of(owner)`: how many tokens `owner` holds.
    pub fn balance_of(&self, owner: &[u8; 32]) -> Result<u64, Vec<u8>> {
        self.view("balance_of", &encode(owner))
    }

    /// `token_uri(token_id)`.
    pub fn token_uri(&self, token_id: &TokenId) -> Result<Option<String>, Vec<u8>> {
        self.view("token_uri", &encode(token_id))
    }

    /// `transfer(to, token_id)` of a token this contract owns.
    pub fn transfer(&self, to: &[u8; 32], token_id: &TokenId) {
        crate::call(&self.0, "transfer", &encode(&(*to, *token_id)));
    }

    /// `transfer_from(from, to, token_id)`: this contract is the owner, the token's approved account or an operator
    /// of `from`.
    pub fn transfer_from(&self, from: &[u8; 32], to: &[u8; 32], token_id: &TokenId) {
        crate::call(&self.0, "transfer_from", &encode(&(*from, *to, *token_id)));
    }

    /// `approve(spender, token_id)` of a token this contract owns or operates.
    pub fn approve(&self, spender: Option<&[u8; 32]>, token_id: &TokenId) {
        crate::call(&self.0, "approve", &encode(&(spender.copied(), *token_id)));
    }

    /// `set_operator(operator, approved, expires_at)` for this contract's tokens.
    pub fn set_operator(&self, operator: &[u8; 32], approved: bool, expires_at: Option<u64>) {
        crate::call(
            &self.0,
            "set_operator",
            &encode(&(*operator, approved, expires_at)),
        );
    }

    /// `transfer_and_call(to, token_id, data)` of a token this contract owns, attaching `funds` from this contract's
    /// account (the collection passes them on to `to`); returns the content of the `Bytes` that `to`'s
    /// `on_nft_received` answered.
    pub fn transfer_and_call(
        &self,
        to: &[u8; 32],
        token_id: &TokenId,
        data: &[u8],
        funds: &[Funds],
    ) -> Result<Vec<u8>, Vec<u8>> {
        let args = encode(&(*to, *token_id, data));
        decode_return(&crate::call_with_funds(
            &self.0,
            "transfer_and_call",
            &args,
            funds,
        ))
    }

    /// [`Collection::transfer`], returning the collection's refusal instead of failing the op.
    pub fn try_transfer(&self, to: &[u8; 32], token_id: &TokenId) -> Result<(), CallError> {
        crate::try_call(&self.0, "transfer", &encode(&(*to, *token_id))).map(|_| ())
    }

    /// [`Collection::transfer_from`], returning the collection's refusal instead of failing the op.
    pub fn try_transfer_from(
        &self,
        from: &[u8; 32],
        to: &[u8; 32],
        token_id: &TokenId,
    ) -> Result<(), CallError> {
        crate::try_call(&self.0, "transfer_from", &encode(&(*from, *to, *token_id))).map(|_| ())
    }
}

/// What `transfer_and_call` gives the recipient's `on_nft_received(from, token_id, data)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    /// The collection: the caller of `on_nft_received`. Only when it is a collection this contract accepts are `from`
    /// and `token_id` true: anyone can call `on_nft_received` directly with made-up values.
    pub collection: [u8; 32],
    /// The account that sent the token, its owner until then (only the owner calls `transfer_and_call`).
    pub from: [u8; 32],
    /// The token this contract now owns.
    pub token_id: TokenId,
    /// The sender's data, which tells this contract what to do.
    pub data: Vec<u8>,
}

/// Decodes the arguments of `on_nft_received` (`args` of the dispatch function). The funds the sender attached to
/// `transfer_and_call` are this call's attached funds ([`crate::call_funds`]). The method answers with
/// [`crate::standard::reply`].
pub fn received(args: &[u8]) -> Result<Received, Vec<u8>> {
    let (from, token_id, data) = decode(args)?;
    Ok(Received {
        collection: crate::caller(),
        from,
        token_id,
        data,
    })
}

/// A KS-721 event: a Borsh tuple with the name first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// `("standards", ids)`: the declaration, the first event of `init`.
    Standards(Vec<String>),
    /// `("metadata", name, symbol)`: emitted by `init` after the declaration, and again when the name or the symbol
    /// changes.
    Metadata {
        /// Name, 1 to 64 bytes.
        name: String,
        /// Symbol, 1 to 16 bytes.
        symbol: String,
    },
    /// `("transfer", from, to, token_id)`: every change of owner; a mint is from the zero account, a burn to it.
    Transfer {
        /// The owner before, the zero account for a mint.
        from: [u8; 32],
        /// The owner now, the zero account for a burn.
        to: [u8; 32],
        /// The token.
        token_id: TokenId,
    },
    /// `("approve", owner, spender, token_id)`: every `approve`; `None` clears the approval. A change of owner clears it
    /// too, without this event.
    Approve {
        /// The token's owner.
        owner: [u8; 32],
        /// The account that may now move the token, if any.
        spender: Option<[u8; 32]>,
        /// The token.
        token_id: TokenId,
    },
    /// `("operator", owner, operator, approved, expires_at)`: every `set_operator`.
    Operator {
        /// The account whose tokens the operator may move.
        owner: [u8; 32],
        /// The operator.
        operator: [u8; 32],
        /// Whether it may (false revokes it).
        approved: bool,
        /// The block timestamp from which it no longer counts; `None` never expires (and always when not approved).
        expires_at: Option<u64>,
    },
    /// `("uri", token_id)`: a token's URI changed after its mint, or every token's (`None`); wallets read it again.
    Uri(Option<TokenId>),
}

impl Event {
    /// Parses an event's bytes; `None` for another event or any other layout (the name and the exact fields).
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut input = data;
        let name = String::decode(&mut input)?;
        let event = match name.as_str() {
            "standards" => Self::Standards(Vec::<String>::decode(&mut input)?),
            "metadata" => {
                let (name, symbol) = <(String, String)>::decode(&mut input)?;
                Self::Metadata { name, symbol }
            }
            "transfer" => {
                let (from, to, token_id) = <([u8; 32], [u8; 32], TokenId)>::decode(&mut input)?;
                Self::Transfer { from, to, token_id }
            }
            "approve" => {
                let (owner, spender, token_id) =
                    <([u8; 32], Option<[u8; 32]>, TokenId)>::decode(&mut input)?;
                Self::Approve {
                    owner,
                    spender,
                    token_id,
                }
            }
            "operator" => {
                let (owner, operator, approved, expires_at) =
                    <([u8; 32], [u8; 32], bool, Option<u64>)>::decode(&mut input)?;
                Self::Operator {
                    owner,
                    operator,
                    approved,
                    expires_at,
                }
            }
            "uri" => Self::Uri(Option::<TokenId>::decode(&mut input)?),
            _ => return None,
        };
        input.is_empty().then_some(event)
    }

    /// The event's bytes.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Standards(ids) => encode(&("standards", ids)),
            Self::Metadata { name, symbol } => encode(&("metadata", name, symbol)),
            Self::Transfer { from, to, token_id } => encode(&("transfer", from, to, token_id)),
            Self::Approve {
                owner,
                spender,
                token_id,
            } => encode(&("approve", owner, *spender, token_id)),
            Self::Operator {
                owner,
                operator,
                approved,
                expires_at,
            } => encode(&("operator", owner, operator, *approved, *expires_at)),
            Self::Uri(token_id) => encode(&("uri", *token_id)),
        }
    }

    /// Records the event.
    pub fn emit(&self) {
        crate::emit_event(&self.encode());
    }
}

/// The rules a collection adds to KS-721: every method defaults to none.
pub trait Policy {
    /// Whether `token_id` may move from `from` to `to`, after the standard's own checks and before every change of
    /// owner: transfers (`transfer`, `transfer_from`, `transfer_and_call`), mints (`from` is the zero account) and
    /// burns (`to` is the zero account). A refusal by the collection's rules should use
    /// [`crate::standard::rejected`]. It may also keep the collection's own records (a listing a change of owner
    /// drops, with its event before the `transfer` event).
    fn check(&self, _from: &[u8; 32], _to: &[u8; 32], _token_id: &TokenId) -> Result<(), Vec<u8>> {
        Ok(())
    }

    /// The capabilities these rules give someone now (names of [`crate::standard::CAPABILITIES`]): `mint` while the
    /// collection can still mint, for example.
    fn capabilities(&self) -> Result<Vec<&'static str>, Vec<u8>> {
        Ok(Vec::new())
    }
}

/// No rules of its own: a plain KS-721 collection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoPolicy;

impl Policy for NoPolicy {}

/// The reference implementation of KS-721, for a collection to build on (see the module docs). It holds no state of
/// its own: a `const` of it is the collection's configuration, the rules ([`Policy`]) and the extensions it implements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ks721<P = NoPolicy> {
    policy: P,
    burnable: bool,
    enumerable: bool,
}

impl Ks721<NoPolicy> {
    /// A plain KS-721 collection, with no extension.
    pub const fn new() -> Self {
        Self {
            policy: NoPolicy,
            burnable: false,
            enumerable: false,
        }
    }

    /// The same collection with the extension `KS-721-burnable` (`burn`).
    pub const fn burnable(self) -> Self {
        Self {
            burnable: true,
            ..self
        }
    }

    /// The same collection with the extension `KS-721-enumerable` (`total_supply`, `tokens`, `tokens_of`): every mint,
    /// transfer and burn keeps the lists too, which costs gas.
    pub const fn enumerable(self) -> Self {
        Self {
            enumerable: true,
            ..self
        }
    }

    /// The same collection with the rules `policy` (after the extensions).
    pub const fn with_policy<Q: Policy>(self, policy: Q) -> Ks721<Q> {
        Ks721 {
            policy,
            burnable: self.burnable,
            enumerable: self.enumerable,
        }
    }
}

fn key(prefix: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

fn read_count(key: &[u8]) -> Result<u64, Vec<u8>> {
    Ok(storage::read::<u64>(key)?.unwrap_or(0))
}

/// Writes a count; 0 removes the key, so that each state has exactly one representation.
fn write_count(key: &[u8], count: u64) {
    if count == 0 {
        storage::remove(key);
    } else {
        storage::write(key, &count);
    }
}

/// A dense list in storage: the item at each position under `prefix ‖ list ‖ position` (big-endian), the position of
/// each item under `index ‖ item`, the length under `len`. Removing an item moves the last one into its place.
struct List<'a> {
    prefix: &'a [u8],
    list: &'a [u8],
    index: &'a [u8],
    len: &'a [u8],
}

impl List<'_> {
    fn slot(&self, position: u64) -> Vec<u8> {
        key(self.prefix, &[self.list, &position.to_be_bytes()])
    }

    fn position_key(&self, item: &TokenId) -> Vec<u8> {
        key(self.index, &[item.as_slice()])
    }

    fn len(&self) -> Result<u64, Vec<u8>> {
        read_count(self.len)
    }

    fn push(&self, item: &TokenId) -> Result<(), Vec<u8>> {
        let len = self.len()?;
        storage::write(&self.slot(len), item);
        storage::write(&self.position_key(item), &len);
        write_count(
            self.len,
            len.checked_add(1).ok_or_else(|| ERR_OVERFLOW.to_vec())?,
        );
        Ok(())
    }

    fn remove(&self, item: &TokenId) -> Result<(), Vec<u8>> {
        let position_key = self.position_key(item);
        let position: u64 =
            storage::read(&position_key)?.ok_or_else(|| ERR_MISSING_INDEX.to_vec())?;
        let last = self
            .len()?
            .checked_sub(1)
            .ok_or_else(|| ERR_MISSING_INDEX.to_vec())?;
        if position != last {
            let moved: TokenId =
                storage::read(&self.slot(last))?.ok_or_else(|| ERR_MISSING_INDEX.to_vec())?;
            storage::write(&self.slot(position), &moved);
            storage::write(&self.position_key(&moved), &position);
        }
        storage::remove(&self.slot(last));
        storage::remove(&position_key);
        write_count(self.len, last);
        Ok(())
    }

    fn page(&self, start: u64, limit: u32) -> Result<Vec<TokenId>, Vec<u8>> {
        let end = self
            .len()?
            .min(start.saturating_add(u64::from(limit.min(MAX_PAGE))));
        (start..end)
            .map(|position| {
                storage::read(&self.slot(position))?.ok_or_else(|| ERR_MISSING_INDEX.to_vec())
            })
            .collect()
    }
}

fn balance_key(owner: &[u8; 32]) -> Vec<u8> {
    key(PREFIX_BALANCE, &[owner.as_slice()])
}

/// Every token, by position (`KS-721-enumerable`).
fn all_tokens() -> List<'static> {
    List {
        prefix: PREFIX_ALL,
        list: &[],
        index: PREFIX_ALL_POSITION,
        len: KEY_SUPPLY,
    }
}

/// The tokens of `owner`, by position (`KS-721-enumerable`); its length is the owner's balance.
fn owned_tokens<'a>(owner: &'a [u8; 32], balance: &'a [u8]) -> List<'a> {
    List {
        prefix: PREFIX_OWNED,
        list: owner.as_slice(),
        index: PREFIX_OWNED_POSITION,
        len: balance,
    }
}

impl<P: Policy> Ks721<P> {
    /// The collection's rules.
    pub fn policy(&self) -> &P {
        &self.policy
    }

    /// The standards the collection declares: `KS-721`, then its extensions.
    pub fn standards(&self) -> Vec<String> {
        let mut ids = Vec::from([String::from(ID)]);
        if self.burnable {
            ids.push(String::from(BURNABLE));
        }
        if self.enumerable {
            ids.push(String::from(ENUMERABLE));
        }
        ids
    }

    /// The collection's `init`: checks and records the name and the symbol and emits the declaration and the metadata,
    /// before any mint (KS-721 rule 1).
    pub fn init(&self, name: &str, symbol: &str) -> Result<(), Vec<u8>> {
        if storage::get(KEY_METADATA).is_some() {
            return Err(ERR_ALREADY_INITIALIZED.to_vec());
        }
        check_name(name)?;
        check_symbol(symbol)?;
        storage::write(KEY_METADATA, &(name, symbol));
        Event::Standards(self.standards()).emit();
        Event::Metadata {
            name: String::from(name),
            symbol: String::from(symbol),
        }
        .emit();
        Ok(())
    }

    /// Answers the standard's methods (and those of the extensions the collection has); `None` for any other method,
    /// `init` and minting included, which the collection's own dispatch function then answers.
    pub fn dispatch(&self, method: &str, args: &[u8]) -> Option<CallResult> {
        let result = match method {
            STANDARDS => run(args, |()| Ok(self.standards())),
            METADATA => run(args, |()| self.metadata()),
            CAPABILITIES_VIEW => run(args, |()| self.capabilities()),
            "owner_of" => run(args, |token_id: TokenId| self.owner_of(&token_id)),
            "balance_of" => run(args, |owner: [u8; 32]| self.balance_of(&owner)),
            "token_uri" => run(args, |token_id: TokenId| self.token_uri(&token_id)),
            "approved" => run(args, |token_id: TokenId| self.approved(&token_id)),
            "is_operator" => run(args, |(owner, operator): ([u8; 32], [u8; 32])| {
                self.is_operator(&owner, &operator)
            }),
            "operator_info" => run(args, |(owner, operator): ([u8; 32], [u8; 32])| {
                self.operator_info(&owner, &operator)
            }),
            "transfer" => run(args, |(to, token_id): ([u8; 32], TokenId)| {
                self.transfer(&crate::caller(), &to, &token_id)
            }),
            "transfer_from" => run(
                args,
                |(from, to, token_id): ([u8; 32], [u8; 32], TokenId)| {
                    self.transfer_from(&crate::caller(), &from, &to, &token_id)
                },
            ),
            "approve" => run(args, |(spender, token_id): (Option<[u8; 32]>, TokenId)| {
                self.approve(&crate::caller(), spender.as_ref(), &token_id)
            }),
            "set_operator" => run(
                args,
                |(operator, approved, expires_at): ([u8; 32], bool, Option<u64>)| {
                    self.set_operator(&crate::caller(), &operator, approved, expires_at)
                },
            ),
            // the one payable method: the funds go on to the recipient
            "transfer_and_call" => self.dispatch_transfer_and_call(args),
            "burn" if self.burnable => run(args, |token_id: TokenId| {
                let owner = self.owner(&token_id)?;
                if !self.may_move(&crate::caller(), &owner, &token_id)? {
                    return Err(ERR_NOT_AUTHORIZED.to_vec());
                }
                self.burn(&token_id)
            }),
            "total_supply" if self.enumerable => run(args, |()| self.total_supply()),
            "tokens" if self.enumerable => {
                run(args, |(start, limit): (u64, u32)| self.tokens(start, limit))
            }
            "tokens_of" if self.enumerable => {
                run(args, |(owner, start, limit): ([u8; 32], u64, u32)| {
                    self.tokens_of(&owner, start, limit)
                })
            }
            _ => return None,
        };
        Some(result)
    }

    /// `metadata() -> (name, symbol)`.
    pub fn metadata(&self) -> Result<(String, String), Vec<u8>> {
        storage::read(KEY_METADATA)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
    }

    /// Changes the name and the symbol and emits `metadata` again: wallets flag the change.
    pub fn set_metadata(&self, name: &str, symbol: &str) -> Result<(), Vec<u8>> {
        check_name(name)?;
        check_symbol(symbol)?;
        self.metadata()?;
        storage::write(KEY_METADATA, &(name, symbol));
        Event::Metadata {
            name: String::from(name),
            symbol: String::from(symbol),
        }
        .emit();
        Ok(())
    }

    /// `capabilities()`: what the [`Policy`] reports, in the order of [`crate::standard::CAPABILITIES`] (names it does
    /// not know last).
    pub fn capabilities(&self) -> Result<Vec<String>, Vec<u8>> {
        Ok(ordered(&self.policy.capabilities()?))
    }

    /// `owner_of(token_id)`: `None` for a token that does not exist.
    pub fn owner_of(&self, token_id: &TokenId) -> Result<Option<[u8; 32]>, Vec<u8>> {
        storage::read(&key(PREFIX_OWNER, &[token_id.as_slice()]))
    }

    /// The owner of a token that must exist.
    fn owner(&self, token_id: &TokenId) -> Result<[u8; 32], Vec<u8>> {
        self.owner_of(token_id)?
            .ok_or_else(|| ERR_UNKNOWN_TOKEN.to_vec())
    }

    /// `balance_of(owner)`: how many tokens `owner` holds.
    pub fn balance_of(&self, owner: &[u8; 32]) -> Result<u64, Vec<u8>> {
        read_count(&balance_key(owner))
    }

    /// `token_uri(token_id)`: `None` for a token without one, or that does not exist.
    pub fn token_uri(&self, token_id: &TokenId) -> Result<Option<String>, Vec<u8>> {
        storage::read(&key(PREFIX_URI, &[token_id.as_slice()]))
    }

    /// `approved(token_id)`: the account that may move the token besides its owner and the owner's operators.
    pub fn approved(&self, token_id: &TokenId) -> Result<Option<[u8; 32]>, Vec<u8>> {
        storage::read(&key(PREFIX_APPROVED, &[token_id.as_slice()]))
    }

    /// `operator_info(owner, operator) -> (approved, expires_at)` as set, expired or not; `(false, None)` when there
    /// is none.
    pub fn operator_info(
        &self,
        owner: &[u8; 32],
        operator: &[u8; 32],
    ) -> Result<(bool, Option<u64>), Vec<u8>> {
        let entry: Option<Option<u64>> = storage::read(&key(
            PREFIX_OPERATOR,
            &[owner.as_slice(), operator.as_slice()],
        ))?;
        Ok(entry.map_or((false, None), |expires_at| (true, expires_at)))
    }

    /// `is_operator(owner, operator)`: whether `operator` may move every token of `owner` now (false once it expired).
    pub fn is_operator(&self, owner: &[u8; 32], operator: &[u8; 32]) -> Result<bool, Vec<u8>> {
        let (approved, expires_at) = self.operator_info(owner, operator)?;
        Ok(approved && !expired(expires_at))
    }

    /// Whether the ID was burned: it is never minted again.
    pub fn is_burned(&self, token_id: &TokenId) -> bool {
        storage::get(&key(PREFIX_BURNED, &[token_id.as_slice()])).is_some()
    }

    /// Whether `account` may move `owner`'s token `token_id`: the owner itself, the token's approved account, or an
    /// operator of the owner.
    pub fn may_move(
        &self,
        account: &[u8; 32],
        owner: &[u8; 32],
        token_id: &TokenId,
    ) -> Result<bool, Vec<u8>> {
        Ok(account == owner
            || self.approved(token_id)? == Some(*account)
            || self.is_operator(owner, account)?)
    }

    /// Moves `owner`'s token `token_id` to `to` once the caller is checked: the recipient, the [`Policy`], the records,
    /// the event.
    fn move_token(
        &self,
        owner: &[u8; 32],
        to: &[u8; 32],
        token_id: &TokenId,
    ) -> Result<(), Vec<u8>> {
        check_recipient(to)?;
        if to == owner {
            return Err(ERR_ALREADY_THE_OWNER.to_vec());
        }
        self.policy.check(owner, to, token_id)?;
        storage::remove(&key(PREFIX_APPROVED, &[token_id.as_slice()]));
        storage::write(&key(PREFIX_OWNER, &[token_id.as_slice()]), to);
        self.leave(owner, token_id)?;
        self.join(to, token_id)?;
        Event::Transfer {
            from: *owner,
            to: *to,
            token_id: *token_id,
        }
        .emit();
        Ok(())
    }

    /// Takes a token off `owner`'s balance (and list).
    fn leave(&self, owner: &[u8; 32], token_id: &TokenId) -> Result<(), Vec<u8>> {
        let balance = balance_key(owner);
        if self.enumerable {
            return owned_tokens(owner, &balance).remove(token_id);
        }
        let count = read_count(&balance)?
            .checked_sub(1)
            .ok_or_else(|| ERR_MISSING_INDEX.to_vec())?;
        write_count(&balance, count);
        Ok(())
    }

    /// Adds a token to `owner`'s balance (and list).
    fn join(&self, owner: &[u8; 32], token_id: &TokenId) -> Result<(), Vec<u8>> {
        let balance = balance_key(owner);
        if self.enumerable {
            return owned_tokens(owner, &balance).push(token_id);
        }
        let count = read_count(&balance)?
            .checked_add(1)
            .ok_or_else(|| ERR_OVERFLOW.to_vec())?;
        write_count(&balance, count);
        Ok(())
    }

    /// `transfer` by `caller`: the owner moves its token to `to`.
    pub fn transfer(
        &self,
        caller: &[u8; 32],
        to: &[u8; 32],
        token_id: &TokenId,
    ) -> Result<(), Vec<u8>> {
        let owner = self.owner(token_id)?;
        if owner != *caller {
            return Err(ERR_NOT_THE_OWNER.to_vec());
        }
        self.move_token(&owner, to, token_id)
    }

    /// `transfer_from` by `caller`: the owner, the token's approved account or an operator of the owner moves the token
    /// from `from`, which must own it now, to `to`.
    pub fn transfer_from(
        &self,
        caller: &[u8; 32],
        from: &[u8; 32],
        to: &[u8; 32],
        token_id: &TokenId,
    ) -> Result<(), Vec<u8>> {
        let owner = self.owner(token_id)?;
        if owner != *from {
            return Err(ERR_WRONG_OWNER.to_vec());
        }
        if !self.may_move(caller, &owner, token_id)? {
            return Err(ERR_NOT_AUTHORIZED.to_vec());
        }
        self.move_token(&owner, to, token_id)
    }

    /// `transfer_and_call` by `caller`: the owner moves its token to the contract `to`, then calls `to`'s
    /// `on_nft_received(caller, token_id, data)` passing on the funds attached to this call, and returns the content of
    /// the `Bytes` it answered. When `to` is no contract, refuses or answers anything but `Bytes`, the whole op fails
    /// and nothing moves.
    pub fn transfer_and_call(
        &self,
        caller: &[u8; 32],
        to: &[u8; 32],
        token_id: &TokenId,
        data: &[u8],
    ) -> Result<Vec<u8>, Vec<u8>> {
        // checks and effects first, then the call (the recipient may call this collection back)
        self.transfer(caller, to, token_id)?;
        let funds = crate::call_funds();
        let answer = crate::call_with_funds(
            to,
            ON_RECEIVED,
            &encode(&(*caller, *token_id, data)),
            &funds,
        );
        decode_return(&answer)
    }

    /// The method `transfer_and_call`: its answer is the recipient's, the same `Bytes`.
    fn dispatch_transfer_and_call(&self, args: &[u8]) -> CallResult {
        let (to, token_id, data): ([u8; 32], TokenId, Vec<u8>) = decode(args)?;
        let content = self.transfer_and_call(&crate::caller(), &to, &token_id, &data)?;
        crate::ok(&content)
    }

    /// `approve` by `caller`, the owner or an operator of the owner: sets the token's one approved account, or clears
    /// it with `None`. Emits `approve`.
    pub fn approve(
        &self,
        caller: &[u8; 32],
        spender: Option<&[u8; 32]>,
        token_id: &TokenId,
    ) -> Result<(), Vec<u8>> {
        let owner = self.owner(token_id)?;
        if owner != *caller && !self.is_operator(&owner, caller)? {
            return Err(ERR_NOT_AUTHORIZED.to_vec());
        }
        let slot = key(PREFIX_APPROVED, &[token_id.as_slice()]);
        match spender {
            Some(spender) if *spender == ZERO_ACCOUNT => return Err(ERR_ZERO_ACCOUNT.to_vec()),
            Some(spender) if *spender == owner => return Err(ERR_SPENDER_IS_OWNER.to_vec()),
            Some(spender) => storage::write(&slot, spender),
            None => storage::remove(&slot),
        }
        Event::Approve {
            owner,
            spender: spender.copied(),
            token_id: *token_id,
        }
        .emit();
        Ok(())
    }

    /// `set_operator` by `owner`: lets `operator` move, approve and burn every token of `owner`, present and future,
    /// until `expires_at` (`None` never expires), or takes that back (`approved` false, which drops the expiry).
    /// Emits `operator`.
    pub fn set_operator(
        &self,
        owner: &[u8; 32],
        operator: &[u8; 32],
        approved: bool,
        expires_at: Option<u64>,
    ) -> Result<(), Vec<u8>> {
        if *operator == ZERO_ACCOUNT {
            return Err(ERR_ZERO_ACCOUNT.to_vec());
        }
        if operator == owner {
            return Err(ERR_OPERATOR_IS_CALLER.to_vec());
        }
        let slot = key(PREFIX_OPERATOR, &[owner.as_slice(), operator.as_slice()]);
        let expires_at = if approved { expires_at } else { None };
        if approved {
            storage::write(&slot, &expires_at);
        } else {
            storage::remove(&slot);
        }
        Event::Operator {
            owner: *owner,
            operator: *operator,
            approved,
            expires_at,
        }
        .emit();
        Ok(())
    }

    /// Creates the token `token_id` for `to`, with its URI (the [`Policy`] checks it as a move from the zero account);
    /// the collection's own `mint` method checks who may mint first. An ID a token has now, or had before it was
    /// burned, is refused.
    pub fn mint(
        &self,
        to: &[u8; 32],
        token_id: &TokenId,
        uri: Option<&str>,
    ) -> Result<(), Vec<u8>> {
        check_recipient(to)?;
        if self.owner_of(token_id)?.is_some() {
            return Err(ERR_TOKEN_EXISTS.to_vec());
        }
        if self.is_burned(token_id) {
            return Err(ERR_TOKEN_BURNED.to_vec());
        }
        if uri.is_some_and(|uri| uri.len() > MAX_URI_BYTES) {
            return Err(ERR_URI_TOO_LONG.to_vec());
        }
        self.policy.check(&ZERO_ACCOUNT, to, token_id)?;
        storage::write(&key(PREFIX_OWNER, &[token_id.as_slice()]), to);
        if let Some(uri) = uri {
            storage::write(&key(PREFIX_URI, &[token_id.as_slice()]), uri);
        }
        self.join(to, token_id)?;
        if self.enumerable {
            all_tokens().push(token_id)?;
        }
        Event::Transfer {
            from: ZERO_ACCOUNT,
            to: *to,
            token_id: *token_id,
        }
        .emit();
        Ok(())
    }

    /// Destroys the token `token_id` (the [`Policy`] checks it as a move to the zero account); `burn` of
    /// `KS-721-burnable` checks that the caller may move the token first. The ID is never minted again.
    pub fn burn(&self, token_id: &TokenId) -> Result<(), Vec<u8>> {
        let owner = self.owner(token_id)?;
        self.policy.check(&owner, &ZERO_ACCOUNT, token_id)?;
        storage::remove(&key(PREFIX_APPROVED, &[token_id.as_slice()]));
        storage::remove(&key(PREFIX_OWNER, &[token_id.as_slice()]));
        storage::remove(&key(PREFIX_URI, &[token_id.as_slice()]));
        storage::write(&key(PREFIX_BURNED, &[token_id.as_slice()]), &());
        self.leave(&owner, token_id)?;
        if self.enumerable {
            all_tokens().remove(token_id)?;
        }
        Event::Transfer {
            from: owner,
            to: ZERO_ACCOUNT,
            token_id: *token_id,
        }
        .emit();
        Ok(())
    }

    /// Changes the URI of an existing token (`None` removes it) and emits `uri`: wallets read it again.
    pub fn set_token_uri(&self, token_id: &TokenId, uri: Option<&str>) -> Result<(), Vec<u8>> {
        self.owner(token_id)?;
        let slot = key(PREFIX_URI, &[token_id.as_slice()]);
        match uri {
            Some(uri) if uri.len() > MAX_URI_BYTES => return Err(ERR_URI_TOO_LONG.to_vec()),
            Some(uri) => storage::write(&slot, uri),
            None => storage::remove(&slot),
        }
        Event::Uri(Some(*token_id)).emit();
        Ok(())
    }

    /// `total_supply()` (`KS-721-enumerable`): how many tokens exist now. 0 for a collection that is not enumerable,
    /// which keeps no count.
    pub fn total_supply(&self) -> Result<u64, Vec<u8>> {
        all_tokens().len()
    }

    /// `tokens(start, limit)` (`KS-721-enumerable`): the tokens at positions `start`, `start + 1`, ... of the list of
    /// every token, at most `limit` (and at most [`MAX_PAGE`]). A burn moves the last token into the burned one's
    /// place, so positions stay dense but change: page again from 0 after a change.
    pub fn tokens(&self, start: u64, limit: u32) -> Result<Vec<TokenId>, Vec<u8>> {
        all_tokens().page(start, limit)
    }

    /// `tokens_of(owner, start, limit)` (`KS-721-enumerable`): the same over the list of `owner`'s tokens, which a
    /// transfer changes the same way.
    pub fn tokens_of(
        &self,
        owner: &[u8; 32],
        start: u64,
        limit: u32,
    ) -> Result<Vec<TokenId>, Vec<u8>> {
        let balance = balance_key(owner);
        owned_tokens(owner, &balance).page(start, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{self, Ks721Subject, ALICE, BOB};
    use crate::mock::{self, Failure};
    use crate::standard::ERR_UNKNOWN_METHOD;
    use alloc::vec;

    const HOLDER: [u8; 32] = [0x11; 32];

    const FULL: Ks721 = Ks721::new().burnable().enumerable();
    const PLAIN: Ks721 = Ks721::new();

    /// A collection on `nft`: `init` mints serial numbers 1 to 3 to the deployer, `mint(to, serial)` anyone may call.
    fn collection(nft: &Ks721, method: &str, args: &[u8]) -> CallResult {
        match method {
            "init" => {
                nft.init("Collection ü", "COL")?;
                for n in 1..=3 {
                    nft.mint(&crate::caller(), &id(n), Some("ipfs://token"))?;
                }
                crate::ok(&())
            }
            "mint" => {
                let (to, n): ([u8; 32], u128) = decode(args)?;
                nft.mint(&to, &id(n), None)?;
                crate::ok(&())
            }
            _ => nft
                .dispatch(method, args)
                .unwrap_or_else(|| Err(ERR_UNKNOWN_METHOD.to_vec())),
        }
    }

    fn deploy(nft: &'static Ks721) -> [u8; 32] {
        mock::deploy(
            HOLDER,
            0,
            b"collection",
            move |m, a| collection(nft, m, a),
            &[],
        )
        .unwrap()
    }

    fn subject(nft: &'static Ks721) -> Ks721Subject {
        Ks721Subject {
            collection: deploy(nft),
            holder: HOLDER,
            tokens: vec![id(1), id(2), id(3)],
        }
    }

    fn reason(result: Result<Vec<u8>, Failure>) -> Vec<u8> {
        match result {
            Err(Failure::Revert { reason, .. }) => reason,
            other => panic!("expected a revert, got {other:?}"),
        }
    }

    fn query<T: Decode>(c: [u8; 32], method: &str, args: &[u8]) -> T {
        decode(&mock::call(BOB, c, method, args).unwrap()).unwrap()
    }

    #[test]
    fn the_reference_collection_conforms() {
        conformance::ks721(|| subject(&FULL)).unwrap();
        conformance::ks721(|| subject(&PLAIN)).unwrap();
    }

    #[test]
    fn ids_are_minted_once() {
        mock::reset();
        let c = deploy(&FULL);
        let mint = |n: u128| mock::call(BOB, c, "mint", &encode(&(ALICE, n)));
        assert_eq!(reason(mint(1)), ERR_TOKEN_EXISTS);
        mock::call(HOLDER, c, "burn", &encode(&id(1))).unwrap();
        assert_eq!(reason(mint(1)), ERR_TOKEN_BURNED);
        mint(4).unwrap();
        assert_eq!(
            query::<Option<[u8; 32]>>(c, "owner_of", &encode(&id(4))),
            Some(ALICE)
        );
        assert_eq!(
            query::<Option<String>>(c, "token_uri", &encode(&id(1))),
            None
        );
        assert_eq!(
            query::<Option<String>>(c, "token_uri", &encode(&id(2))),
            Some(String::from("ipfs://token"))
        );
    }

    #[test]
    fn the_lists_stay_dense() {
        mock::reset();
        let c = deploy(&FULL);
        // burning the first token moves the last into its place
        mock::call(HOLDER, c, "burn", &encode(&id(1))).unwrap();
        assert_eq!(query::<u64>(c, "total_supply", &[]), 2);
        assert_eq!(
            query::<Vec<TokenId>>(c, "tokens", &encode(&(0u64, 10u32))),
            [id(3), id(2)]
        );
        mock::call(HOLDER, c, "transfer", &encode(&(ALICE, id(3)))).unwrap();
        assert_eq!(
            query::<Vec<TokenId>>(c, "tokens_of", &encode(&(HOLDER, 0u64, 10u32))),
            [id(2)]
        );
        assert_eq!(
            query::<Vec<TokenId>>(c, "tokens_of", &encode(&(ALICE, 0u64, 10u32))),
            [id(3)]
        );
        assert!(query::<Vec<TokenId>>(c, "tokens", &encode(&(1u64, 0u32))).is_empty());
        assert!(query::<Vec<TokenId>>(c, "tokens", &encode(&(5u64, 10u32))).is_empty());
        assert_eq!(query::<u64>(c, "balance_of", &encode(&HOLDER)), 1);
        // a collection that is not enumerable has none of the methods
        let plain = mock::deploy(
            HOLDER,
            1,
            b"collection",
            |m, a| collection(&PLAIN, m, a),
            &[],
        )
        .unwrap();
        assert_eq!(
            reason(mock::call(BOB, plain, "total_supply", &[])),
            ERR_UNKNOWN_METHOD
        );
        assert_eq!(
            reason(mock::call(HOLDER, plain, "burn", &encode(&id(1)))),
            ERR_UNKNOWN_METHOD
        );
        assert_eq!(query::<u64>(plain, "balance_of", &encode(&HOLDER)), 3);
    }

    #[test]
    fn uris_change_with_an_event() {
        mock::reset();
        mock::with_context(HOLDER, [0x44; 32], || {
            FULL.init("C", "C").unwrap();
            FULL.mint(&HOLDER, &id(1), Some("a")).unwrap();
            let long = "u".repeat(MAX_URI_BYTES + 1);
            assert_eq!(
                FULL.mint(&HOLDER, &id(2), Some(&long)),
                Err(ERR_URI_TOO_LONG.to_vec())
            );
            assert_eq!(
                FULL.set_token_uri(&id(2), Some("b")),
                Err(ERR_UNKNOWN_TOKEN.to_vec())
            );
            FULL.set_token_uri(&id(1), Some("b")).unwrap();
            assert_eq!(FULL.token_uri(&id(1)), Ok(Some(String::from("b"))));
            FULL.set_token_uri(&id(1), None).unwrap();
            assert_eq!(FULL.token_uri(&id(1)), Ok(None));
        });
        let uris: Vec<Event> = mock::events()
            .into_iter()
            .filter_map(|e| Event::parse(&e.data))
            .filter(|e| matches!(e, Event::Uri(_)))
            .collect();
        assert_eq!(uris, [Event::Uri(Some(id(1))), Event::Uri(Some(id(1)))]);
    }

    #[test]
    fn approvals_refuse_the_owner_and_operators_the_caller() {
        mock::reset();
        let c = deploy(&FULL);
        assert_eq!(
            reason(mock::call(
                HOLDER,
                c,
                "approve",
                &encode(&(Some(HOLDER), id(1)))
            )),
            ERR_SPENDER_IS_OWNER
        );
        assert_eq!(
            reason(mock::call(
                HOLDER,
                c,
                "set_operator",
                &encode(&(HOLDER, true, None::<u64>))
            )),
            ERR_OPERATOR_IS_CALLER
        );
        // an operator burns, and an approval moves no other token
        mock::call(
            HOLDER,
            c,
            "set_operator",
            &encode(&(BOB, true, None::<u64>)),
        )
        .unwrap();
        mock::call(BOB, c, "burn", &encode(&id(2))).unwrap();
        mock::call(HOLDER, c, "approve", &encode(&(Some(ALICE), id(1)))).unwrap();
        assert_eq!(
            reason(mock::call(
                ALICE,
                c,
                "transfer_from",
                &encode(&(HOLDER, ALICE, id(3)))
            )),
            ERR_NOT_AUTHORIZED
        );
    }

    #[test]
    fn serial_numbers_are_big_endian() {
        assert_eq!(id(0), [0; 32]);
        let twelve = id(12);
        assert_eq!(twelve[31], 12);
        assert!(twelve[..31].iter().all(|b| *b == 0));
        assert_eq!(id(0x0102)[30..], [1, 2]);
        assert!(id(1) < id(2) && id(255) < id(256));
        assert_eq!(serial(&id(u128::MAX)), Some(u128::MAX));
        assert_eq!(serial(&twelve), Some(12));
        let mut hash = [0u8; 32];
        hash[15] = 1;
        assert_eq!(serial(&hash), None);
    }

    #[test]
    fn events_round_trip() {
        let events = [
            Event::Standards(vec![String::from(ID), String::from(ENUMERABLE)]),
            Event::Metadata {
                name: String::from("Collection ṇ"),
                symbol: String::from("COL"),
            },
            Event::Transfer {
                from: [0; 32],
                to: [2; 32],
                token_id: id(3),
            },
            Event::Approve {
                owner: [1; 32],
                spender: None,
                token_id: id(3),
            },
            Event::Operator {
                owner: [1; 32],
                operator: [2; 32],
                approved: true,
                expires_at: Some(9),
            },
            Event::Uri(None),
        ];
        for event in events {
            assert_eq!(Event::parse(&event.encode()), Some(event.clone()));
            let mut longer = event.encode();
            longer.push(0);
            assert_eq!(Event::parse(&longer), None, "{event:?}");
        }
        let transfer = Event::Transfer {
            from: [1; 32],
            to: [2; 32],
            token_id: id(3),
        };
        assert_eq!(transfer.encode().len(), 4 + 8 + 32 * 3);
        // a KS-20 transfer has the same name and another layout
        let fungible = crate::ks20::Event::Transfer {
            from: [1; 32],
            to: [2; 32],
            amount: 3,
        };
        assert_eq!(Event::parse(&fungible.encode()), None);
    }

    #[test]
    fn standards_list_the_extensions() {
        assert_eq!(Ks721::new().standards(), vec![String::from(ID)]);
        assert_eq!(
            Ks721::new().enumerable().burnable().standards(),
            vec![
                String::from(ID),
                String::from(BURNABLE),
                String::from(ENUMERABLE)
            ]
        );
    }
}
