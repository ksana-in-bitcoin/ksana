//! Example contract: a KS-721 collection of non-fungible tokens with a fixed-price market (SPEC §9.5,
//! `docs/STANDARDS.md` §4).
//!
//! Built-in assets are fungible amounts and the VM cannot say who holds a given unit, so the ledger of which account
//! owns which token lives in this contract's storage. The collection is built on the contract SDK's reference
//! implementation of KS-721 ([`ksana_contract_sdk::ks721`]) with the extensions `KS-721-burnable` and
//! `KS-721-enumerable`: wallets, explorers and other contracts read and move its tokens as they do every KS-721
//! collection's. The minter (at first the deployer) mints tokens, each with an optional metadata URI (an image URL, a
//! `data:` URI or plain text); owners transfer, approve, burn and sell them at a fixed price in a built-in asset (KSA or
//! any other).
//!
//! Arguments and return values are Borsh-encoded. The arguments are a tuple in the order of the table below (a single
//! argument is just itself; no arguments is empty bytes). An account is 32 bytes: a user's Taproot output key or a
//! contract ID. A token ID is 32 bytes too: the `n`-th token minted has the ID `ks721::id(n)`, `n` big-endian in the last
//! 16 bytes, which wallets show as `#n`.
//!
//! The collection's own methods:
//!
//! | Method | Arguments | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `(name: String, symbol: String, max_supply: Option<u64>)` | `()` |
//! | `mint` | `(to: [u8; 32], uri: Option<String>)` | `[u8; 32]` (the new token's ID) |
//! | `list` | `(token_id: [u8; 32], asset: [u8; 32], price: u128)` | `()` |
//! | `unlist` | `token_id: [u8; 32]` | `()` |
//! | `buy` (the price attached) | `token_id: [u8; 32]` | `()` |
//! | `set_minter` | `minter: Option<[u8; 32]>` | `()` |
//! | `collection` | none | [`Collection`] |
//! | `token` | `token_id: [u8; 32]` | `Option<Token>` ([`Token`]) |
//! | `details` | `(start: u64, limit: u32)` | `Vec<Token>` |
//! | `details_of` | `(owner: [u8; 32], start: u64, limit: u32)` | `Vec<Token>` |
//! | `listings` | `(start: u64, limit: u32)` | `Vec<Token>` |
//!
//! and every method of KS-721 and its two extensions, answered by the reference implementation: `standards`, `metadata`,
//! `capabilities`, `owner_of`, `balance_of`, `token_uri`, `approved`, `is_operator`, `operator_info`, `transfer`,
//! `transfer_from`, `approve`, `set_operator`, `transfer_and_call`, `burn`, `total_supply`, `tokens` and `tokens_of`.
//!
//! Rules:
//!
//! - `init`: `name` is 1 to 64 bytes and `symbol` 1 to 16 bytes (UTF-8, counted in bytes); `max_supply`, when given,
//!   is above 0. The deployer becomes the minter. It emits the declaration and the metadata (KS-721 rule 1), then
//!   `minter` from `None` to the deployer.
//! - `mint`: the minter only; `uri` is at most 512 bytes, so that a page of the views below stays within a call's
//!   output. The `n`-th mint creates the token `ks721::id(n)`; `minted` counts every mint and `supply` the tokens not
//!   burned. `max_supply` caps `minted`: burned IDs are never minted again and still count. The collection reports the
//!   capability `mint` while it has a minter and has not reached `max_supply`.
//! - Moving tokens is KS-721's: `transfer` and `transfer_and_call` by the owner, `transfer_from` by the owner, the
//!   token's approved account or an operator of the owner, `burn` by any of them; `approve` by the owner or an
//!   operator; operators until an optional expiry (`docs/STANDARDS.md` §4.2).
//! - `list`: the owner only (not the approved account or an operator, so the seller is never in doubt); `price` is
//!   above 0, in base units of `asset` (KSA is 32 zero bytes); listing a listed token replaces its listing. `unlist`:
//!   the owner only, of a listed token.
//! - Every change of owner (a transfer, a sale or a burn) drops the token's listing: the market's own rule, checked by
//!   the collection's [`Market`] policy before the move, which emits `unlist` before the `transfer` event (a sale
//!   takes its listing without an `unlist`).
//! - `buy` is the one payable method besides `transfer_and_call`: the token must be listed and the buyer must not be
//!   the seller; the attached funds must be exactly one item, the listed asset, of exactly the price. The token moves to
//!   the buyer before the price is paid on to the seller with `asset_transfer`, so the contract keeps nothing.
//! - `set_minter`: the minter only; `Some(account)` hands the role over (not to the zero account), `None` ends minting
//!   for good.
//! - Every other method refuses attached funds; the views only read.
//! - Paging: `details` and `details_of` return the tokens of the same pages as KS-721's `tokens` and `tokens_of`, the
//!   tokens at positions `start`, `start + 1`, ... of the list of every token or of the owner's tokens, with each
//!   token's owner, URI, approval and listing; `listings` the tokens at those positions of the listing list. Removing a
//!   token from a list moves the last one into its place, so positions stay dense but change: page again from 0 after a
//!   change. A page of these views holds at most 50 tokens (`limit` is capped at 50), and none for `limit` 0.
//!
//! A sale pays the seller after every change to storage, and `transfer_and_call` calls the recipient after the move
//! (checks-effects-interactions).
//!
//! Events (Borsh tuples, the name first): KS-721's `standards`, `metadata`, `transfer` (mints from the zero account,
//! burns to it), `approve` and `operator`, and the collection's own `("list", seller, token_id, asset, price)`,
//! `("unlist", seller, token_id)`, `("sale", seller, buyer, token_id, asset, price)` and `("minter", previous, minter)`. A
//! sale emits `transfer` and then `sale`.
//!
//! The interface, as the playground reads it (`TokenId` is the playground's name for a 32-byte token ID):
//!
//! ```abi
//! struct Collection { name: String, symbol: String, minter: Option<Account>, minted: u64, supply: u64, max_supply: Option<u64> }
//! struct Listing { asset: AssetId, price: u128 }
//! struct Token { id: TokenId, owner: Account, uri: Option<String>, approved: Option<Account>, listing: Option<Listing> }
//! init(name: String, symbol: String, max_supply: Option<u64>)       // the deployer becomes the minter
//! call mint(to: Account, uri: Option<String>) -> TokenId             // minter only; returns the new token's ID
//! call transfer(to: Account, token_id: TokenId)                      // by the owner
//! call transfer_from(from: Account, to: Account, token_id: TokenId)  // by the owner, the approved account or an operator; from owns it now
//! call approve(spender: Option<Account>, token_id: TokenId)          // by the owner or an operator; None clears
//! call set_operator(operator: Account, approved: bool, expires_at: Option<u64>)   // lets an account move all of the caller's tokens, until expires_at
//! payable call transfer_and_call(to: ContractId, token_id: TokenId, data: Bytes) -> Bytes   // by the owner, to a contract that takes tokens; returns its answer
//! call burn(token_id: TokenId)                                       // by the owner, the approved account or an operator
//! call list(token_id: TokenId, asset: AssetId, price: u128)          // by the owner: offers the token for a fixed price
//! call unlist(token_id: TokenId)                                     // by the owner
//! payable call buy(token_id: TokenId)                                // attach exactly the price in the listed asset
//! call set_minter(minter: Option<Account>)                           // minter only; None ends minting for good
//! view standards() -> Vec<String>
//! view metadata() -> (String, String)
//! view capabilities() -> Vec<String>
//! view collection() -> Collection
//! view token(token_id: TokenId) -> Option<Token>
//! view owner_of(token_id: TokenId) -> Option<Account>
//! view balance_of(owner: Account) -> u64
//! view token_uri(token_id: TokenId) -> Option<String>
//! view approved(token_id: TokenId) -> Option<Account>
//! view is_operator(owner: Account, operator: Account) -> bool
//! view operator_info(owner: Account, operator: Account) -> (bool, Option<u64>)   // (approved, expires_at) as set, expired or not
//! view total_supply() -> u64
//! view tokens(start: u64, limit: u32) -> Vec<TokenId>                     // every token by position, up to 100
//! view tokens_of(owner: Account, start: u64, limit: u32) -> Vec<TokenId>  // the owner's tokens by position, up to 100
//! view details(start: u64, limit: u32) -> Vec<Token>                      // the tokens of a page of tokens(), up to 50
//! view details_of(owner: Account, start: u64, limit: u32) -> Vec<Token>   // the tokens of a page of tokens_of(), up to 50
//! view listings(start: u64, limit: u32) -> Vec<Token>                     // the listed tokens by position, up to 50
//! event standards(ids: Vec<String>)
//! event metadata(name: String, symbol: String)
//! event transfer(from: Account, to: Account, token_id: TokenId)
//! event approve(owner: Account, spender: Option<Account>, token_id: TokenId)
//! event operator(owner: Account, operator: Account, approved: bool, expires_at: Option<u64>)
//! event list(seller: Account, token_id: TokenId, asset: AssetId, price: u128)
//! event unlist(seller: Account, token_id: TokenId)
//! event sale(seller: Account, buyer: Account, token_id: TokenId, asset: AssetId, price: u128)
//! event minter(previous: Option<Account>, minter: Option<Account>)
//! ```

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::borsh::BorshSerialize;
use ksana_contract_sdk::ks721::{self, Ks721, Policy, TokenId};
use ksana_contract_sdk::standard::{self, ZERO_ACCOUNT};
use ksana_contract_sdk::{self as sdk, storage, CallResult, Decode};

sdk::entry!(dispatch);

/// Maximum length of a token's metadata URI, in bytes.
pub const MAX_URI_BYTES: usize = 512;
/// Maximum number of tokens one page of `details`, `details_of` or `listings` holds (`limit` is capped at it).
pub const MAX_PAGE: u32 = 50;

// Storage, besides the reference implementation's (keys that start with `KS721:`). Positions are big-endian `u64`
// inside keys; values are Borsh.

/// The [`Minting`].
const KEY_MINTING: &[u8] = b"minting";
/// `k ‖ token ID` → the token's [`Listing`], while it is listed.
const PREFIX_LISTING: &[u8] = b"k";
/// The number of listed tokens (`u64`), absent at 0.
const KEY_LISTINGS: &[u8] = b"listings";
/// `l ‖ position` → the ID of the token at that position of the listing list.
const PREFIX_LISTED: &[u8] = b"l";
/// `q ‖ token ID` → the token's position in the listing list, while it is listed.
const PREFIX_LISTED_POSITION: &[u8] = b"q";

const ERR_ZERO_MAX_SUPPLY: &[u8] = b"max_supply must be positive";
const ERR_URI: &[u8] = b"uri must be at most 512 bytes";
const ERR_MAX_SUPPLY: &[u8] = b"max supply reached";
const ERR_ZERO_PRICE: &[u8] = b"price must be positive";
const ERR_NOT_LISTED: &[u8] = b"token is not listed";
const ERR_BUYER_IS_SELLER: &[u8] = b"buyer is the seller";
const ERR_PRICE: &[u8] = b"attach exactly the price";

/// The collection's KS-721 implementation: burnable and enumerable, with the market's rule ([`Market`]).
pub const NFT: Ks721<Market> = Ks721::new().burnable().enumerable().with_policy(Market);

/// The collection's rules beside KS-721's: a change of owner drops the token's listing, and the collection reports
/// `mint` while it can still mint.
#[derive(Clone, Copy, Debug)]
pub struct Market;

impl Policy for Market {
    fn check(&self, from: &[u8; 32], _to: &[u8; 32], token_id: &TokenId) -> Result<(), Vec<u8>> {
        // A mint has no listing to drop; a transfer or a burn drops it, with its event before the `transfer` event
        if *from != ZERO_ACCOUNT && drop_listing(token_id)? {
            sdk::emit(&("unlist", *from, *token_id));
        }
        Ok(())
    }

    fn capabilities(&self) -> Result<Vec<&'static str>, Vec<u8>> {
        let mut out = Vec::new();
        if can_mint(&load_minting()?) {
            out.push(standard::CAP_MINT);
        }
        Ok(out)
    }
}

/// Who mints and how far (the value under `minting`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Minting {
    /// The account that may mint; `None` once minting has ended for good.
    pub minter: Option<[u8; 32]>,
    /// Tokens minted so far, burned ones included: the serial number of the last token minted.
    pub minted: u64,
    /// The cap on `minted`, if any.
    pub max_supply: Option<u64>,
}

impl Decode for Minting {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            minter: Option::<[u8; 32]>::decode(input)?,
            minted: u64::decode(input)?,
            max_supply: Option::<u64>::decode(input)?,
        })
    }
}

/// The collection (the return value of `collection`; the Borsh field order is as listed).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Collection {
    /// Name, 1 to 64 bytes.
    pub name: String,
    /// Symbol, 1 to 16 bytes.
    pub symbol: String,
    /// The account that may mint; `None` once minting has ended for good.
    pub minter: Option<[u8; 32]>,
    /// Tokens minted so far, burned ones included: the serial number of the last token minted.
    pub minted: u64,
    /// Tokens not burned.
    pub supply: u64,
    /// The cap on `minted`, if any.
    pub max_supply: Option<u64>,
}

impl Decode for Collection {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            name: String::decode(input)?,
            symbol: String::decode(input)?,
            minter: Option::<[u8; 32]>::decode(input)?,
            minted: u64::decode(input)?,
            supply: u64::decode(input)?,
            max_supply: Option::<u64>::decode(input)?,
        })
    }
}

/// A fixed-price offer of a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Listing {
    /// The asset the price is paid in (KSA is 32 zero bytes).
    pub asset: [u8; 32],
    /// The price in base units of the asset, above 0.
    pub price: u128,
}

impl Decode for Listing {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            asset: <[u8; 32]>::decode(input)?,
            price: u128::decode(input)?,
        })
    }
}

/// A token as the views return it (the Borsh field order is as listed).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Token {
    /// Token ID: `ks721::id(n)` for the `n`-th token minted.
    pub id: TokenId,
    /// Owner account.
    pub owner: [u8; 32],
    /// Metadata URI given at mint, at most 512 bytes: an image URL, a `data:` URI or plain text.
    pub uri: Option<String>,
    /// The one account approved to move or burn this token besides the owner and its operators; cleared when the owner
    /// changes.
    pub approved: Option<[u8; 32]>,
    /// The fixed-price offer, while the token is listed; dropped when the owner changes.
    pub listing: Option<Listing>,
}

impl Decode for Token {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            id: <TokenId>::decode(input)?,
            owner: <[u8; 32]>::decode(input)?,
            uri: Option::<String>::decode(input)?,
            approved: Option::<[u8; 32]>::decode(input)?,
            listing: Option::<Listing>::decode(input)?,
        })
    }
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let (name, symbol, max_supply): (String, String, Option<u64>) = sdk::decode(args)?;
            init(&name, &symbol, max_supply)?;
            sdk::ok(&())
        }
        "mint" => {
            let (to, uri): ([u8; 32], Option<String>) = sdk::decode(args)?;
            sdk::ok(&mint(&to, uri.as_deref())?)
        }
        "list" => {
            let (token_id, asset, price) = sdk::decode(args)?;
            list(&token_id, &asset, price)?;
            sdk::ok(&())
        }
        "unlist" => {
            let token_id = sdk::decode(args)?;
            unlist(&token_id)?;
            sdk::ok(&())
        }
        "buy" => {
            let token_id = sdk::decode(args)?;
            buy(&token_id)?;
            sdk::ok(&())
        }
        "set_minter" => {
            let minter = sdk::decode(args)?;
            set_minter(minter)?;
            sdk::ok(&())
        }
        "collection" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&collection()?)
        }
        "token" => {
            let token_id = sdk::decode(args)?;
            sdk::ok(&token(&token_id)?)
        }
        "details" => {
            let (start, limit) = sdk::decode(args)?;
            sdk::ok(&details(start, limit)?)
        }
        "details_of" => {
            let (owner, start, limit) = sdk::decode(args)?;
            sdk::ok(&details_of(&owner, start, limit)?)
        }
        "listings" => {
            let (start, limit) = sdk::decode(args)?;
            sdk::ok(&listings(start, limit)?)
        }
        // every method of KS-721 and its extensions
        _ => NFT
            .dispatch(method, args)
            .unwrap_or_else(|| sdk::revert("unknown method")),
    }
}

/// Reads who mints and how far; reverts before `init` (which the VM runs at deployment, so only code running in
/// another contract's storage, such as a `delegate_call` of this contract, can get there).
fn load_minting() -> Result<Minting, Vec<u8>> {
    storage::read(KEY_MINTING)?.ok_or_else(|| standard::ERR_NOT_INITIALIZED.to_vec())
}

/// Whether the collection can still mint: it has a minter and has not reached its cap.
fn can_mint(minting: &Minting) -> bool {
    minting.minter.is_some() && minting.max_supply.is_none_or(|max| minting.minted < max)
}

/// Reverts unless the caller is the minter.
fn only_minter(minting: &Minting) -> Result<(), Vec<u8>> {
    match minting.minter {
        None => Err(standard::ERR_MINTING_ENDED.to_vec()),
        Some(minter) if minter == sdk::caller() => Ok(()),
        Some(_) => Err(standard::ERR_NOT_THE_MINTER.to_vec()),
    }
}

/// The owner of a token that must exist.
fn owner(token_id: &TokenId) -> Result<[u8; 32], Vec<u8>> {
    NFT.owner_of(token_id)?
        .ok_or_else(|| ks721::ERR_UNKNOWN_TOKEN.to_vec())
}

fn listing_key(token_id: &TokenId) -> Vec<u8> {
    [PREFIX_LISTING, token_id.as_slice()].concat()
}

fn load_listing(token_id: &TokenId) -> Result<Option<Listing>, Vec<u8>> {
    storage::read(&listing_key(token_id))
}

/// `limit` capped at [`MAX_PAGE`].
fn page_limit(limit: u32) -> u32 {
    limit.min(MAX_PAGE)
}

/// The listing list: the IDs of the listed tokens by position. It keeps its length, the ID at each position and the
/// position of each ID; removing an ID moves the last one into its place (swap-remove), so the positions stay
/// `0..length` and every change costs a few storage operations.
mod listed {
    use super::{KEY_LISTINGS, PREFIX_LISTED, PREFIX_LISTED_POSITION};
    use alloc::vec::Vec;
    use ksana_contract_sdk::ks721::TokenId;
    use ksana_contract_sdk::standard::{ERR_MISSING_INDEX, ERR_OVERFLOW};
    use ksana_contract_sdk::storage;

    fn item_key(position: u64) -> Vec<u8> {
        [PREFIX_LISTED, position.to_be_bytes().as_slice()].concat()
    }

    fn position_key(token_id: &TokenId) -> Vec<u8> {
        [PREFIX_LISTED_POSITION, token_id.as_slice()].concat()
    }

    fn missing() -> Vec<u8> {
        ERR_MISSING_INDEX.to_vec()
    }

    pub fn len() -> Result<u64, Vec<u8>> {
        Ok(storage::read(KEY_LISTINGS)?.unwrap_or(0))
    }

    /// The ID at `position`, which must be below the length.
    pub fn get(position: u64) -> Result<TokenId, Vec<u8>> {
        storage::read(&item_key(position))?.ok_or_else(missing)
    }

    /// Appends `token_id`.
    pub fn push(token_id: &TokenId) -> Result<(), Vec<u8>> {
        let len = len()?;
        let new_len = len.checked_add(1).ok_or_else(|| ERR_OVERFLOW.to_vec())?;
        storage::write(&item_key(len), token_id);
        storage::write(&position_key(token_id), &len);
        storage::write(KEY_LISTINGS, &new_len);
        Ok(())
    }

    /// Removes `token_id`, which must be in the list: the last ID takes its position.
    pub fn remove(token_id: &TokenId) -> Result<(), Vec<u8>> {
        let position_key = position_key(token_id);
        let position: u64 = storage::read(&position_key)?.ok_or_else(missing)?;
        let last = len()?.checked_sub(1).ok_or_else(missing)?;
        if position != last {
            let moved = get(last)?;
            storage::write(&item_key(position), &moved);
            storage::write(&self::position_key(&moved), &position);
        }
        storage::remove(&item_key(last));
        storage::remove(&position_key);
        if last == 0 {
            storage::remove(KEY_LISTINGS);
        } else {
            storage::write(KEY_LISTINGS, &last);
        }
        Ok(())
    }
}

/// Takes the token's listing away, out of the listing list too; returns whether it was listed.
fn drop_listing(token_id: &TokenId) -> Result<bool, Vec<u8>> {
    if load_listing(token_id)?.is_none() {
        return Ok(false);
    }
    storage::remove(&listing_key(token_id));
    listed::remove(token_id)?;
    Ok(true)
}

/// `init(name, symbol, max_supply)`: declares the collection, with the deployer as the minter.
pub fn init(name: &str, symbol: &str, max_supply: Option<u64>) -> Result<(), Vec<u8>> {
    standard::no_funds()?;
    if max_supply == Some(0) {
        return Err(ERR_ZERO_MAX_SUPPLY.to_vec());
    }
    NFT.init(name, symbol)?;
    let minter = sdk::caller();
    storage::write(
        KEY_MINTING,
        &Minting {
            minter: Some(minter),
            minted: 0,
            max_supply,
        },
    );
    sdk::emit(&("minter", None::<[u8; 32]>, Some(minter)));
    Ok(())
}

/// `mint(to, uri) -> token_id`: the minter mints the next token to `to`.
pub fn mint(to: &[u8; 32], uri: Option<&str>) -> Result<TokenId, Vec<u8>> {
    standard::no_funds()?;
    let mut minting = load_minting()?;
    only_minter(&minting)?;
    if uri.is_some_and(|uri| uri.len() > MAX_URI_BYTES) {
        return Err(ERR_URI.to_vec());
    }
    if minting.max_supply.is_some_and(|max| minting.minted >= max) {
        return Err(ERR_MAX_SUPPLY.to_vec());
    }
    let serial = minting
        .minted
        .checked_add(1)
        .ok_or_else(|| standard::ERR_OVERFLOW.to_vec())?;
    let token_id = ks721::id(u128::from(serial));
    NFT.mint(to, &token_id, uri)?;
    minting.minted = serial;
    storage::write(KEY_MINTING, &minting);
    Ok(token_id)
}

/// `list(token_id, asset, price)`: the owner offers a token for `price` base units of `asset`, replacing the token's
/// listing if it has one.
pub fn list(token_id: &TokenId, asset: &[u8; 32], price: u128) -> Result<(), Vec<u8>> {
    standard::no_funds()?;
    let seller = owner(token_id)?;
    if sdk::caller() != seller {
        return Err(ks721::ERR_NOT_THE_OWNER.to_vec());
    }
    if price == 0 {
        return Err(ERR_ZERO_PRICE.to_vec());
    }
    if load_listing(token_id)?.is_none() {
        listed::push(token_id)?;
    }
    storage::write(
        &listing_key(token_id),
        &Listing {
            asset: *asset,
            price,
        },
    );
    sdk::emit(&("list", seller, *token_id, *asset, price));
    Ok(())
}

/// `unlist(token_id)`: the owner withdraws the offer of a listed token.
pub fn unlist(token_id: &TokenId) -> Result<(), Vec<u8>> {
    standard::no_funds()?;
    let seller = owner(token_id)?;
    if sdk::caller() != seller {
        return Err(ks721::ERR_NOT_THE_OWNER.to_vec());
    }
    if !drop_listing(token_id)? {
        return Err(ERR_NOT_LISTED.to_vec());
    }
    sdk::emit(&("unlist", seller, *token_id));
    Ok(())
}

/// `buy(token_id)`: the caller buys a listed token with the price attached in the listed asset, which goes on to the
/// seller.
pub fn buy(token_id: &TokenId) -> Result<(), Vec<u8>> {
    let seller = owner(token_id)?;
    let Listing { asset, price } =
        load_listing(token_id)?.ok_or_else(|| ERR_NOT_LISTED.to_vec())?;
    let buyer = sdk::caller();
    if buyer == seller {
        return Err(ERR_BUYER_IS_SELLER.to_vec());
    }
    match sdk::call_funds().as_slice() {
        [funds] if funds.asset == asset && funds.amount == price => {}
        _ => return Err(ERR_PRICE.to_vec()),
    }
    // The sale takes the listing: the move finds none to drop
    drop_listing(token_id)?;
    NFT.transfer(&seller, &buyer, token_id)?;
    sdk::asset_transfer(&asset, &seller, price);
    sdk::emit(&("sale", seller, buyer, *token_id, asset, price));
    Ok(())
}

/// `set_minter(minter)`: the minter hands the role over, or with `None` ends minting for good.
pub fn set_minter(minter: Option<[u8; 32]>) -> Result<(), Vec<u8>> {
    standard::no_funds()?;
    let mut minting = load_minting()?;
    only_minter(&minting)?;
    if minter == Some(ZERO_ACCOUNT) {
        return Err(standard::ERR_ZERO_ACCOUNT.to_vec());
    }
    let previous = minting.minter;
    minting.minter = minter;
    storage::write(KEY_MINTING, &minting);
    sdk::emit(&("minter", previous, minter));
    Ok(())
}

/// `collection()`.
pub fn collection() -> Result<Collection, Vec<u8>> {
    standard::no_funds()?;
    let minting = load_minting()?;
    let (name, symbol) = NFT.metadata()?;
    Ok(Collection {
        name,
        symbol,
        minter: minting.minter,
        minted: minting.minted,
        supply: NFT.total_supply()?,
        max_supply: minting.max_supply,
    })
}

/// The token `token_id` as the views return it; `None` when it was never minted or has been burned.
fn load_token(token_id: &TokenId) -> Result<Option<Token>, Vec<u8>> {
    let Some(owner) = NFT.owner_of(token_id)? else {
        return Ok(None);
    };
    Ok(Some(Token {
        id: *token_id,
        owner,
        uri: NFT.token_uri(token_id)?,
        approved: NFT.approved(token_id)?,
        listing: load_listing(token_id)?,
    }))
}

/// The tokens of a page of IDs, each of which must exist.
fn tokens_of_ids(ids: &[TokenId]) -> Result<Vec<Token>, Vec<u8>> {
    ids.iter()
        .map(|id| load_token(id)?.ok_or_else(|| standard::ERR_MISSING_INDEX.to_vec()))
        .collect()
}

/// `token(token_id)`: `None` when the token was never minted or has been burned.
pub fn token(token_id: &TokenId) -> Result<Option<Token>, Vec<u8>> {
    standard::no_funds()?;
    load_minting()?;
    load_token(token_id)
}

/// `details(start, limit)`: the tokens at positions `start`, `start + 1`, ... of the list of every token.
pub fn details(start: u64, limit: u32) -> Result<Vec<Token>, Vec<u8>> {
    standard::no_funds()?;
    load_minting()?;
    tokens_of_ids(&NFT.tokens(start, page_limit(limit))?)
}

/// `details_of(owner, start, limit)`: the tokens at positions `start`, `start + 1`, ... of `owner`'s list.
pub fn details_of(owner: &[u8; 32], start: u64, limit: u32) -> Result<Vec<Token>, Vec<u8>> {
    standard::no_funds()?;
    load_minting()?;
    tokens_of_ids(&NFT.tokens_of(owner, start, page_limit(limit))?)
}

/// `listings(start, limit)`: the tokens at positions `start`, `start + 1`, ... of the listing list.
pub fn listings(start: u64, limit: u32) -> Result<Vec<Token>, Vec<u8>> {
    standard::no_funds()?;
    load_minting()?;
    let end = listed::len()?.min(start.saturating_add(u64::from(page_limit(limit))));
    let ids = (start..end)
        .map(listed::get)
        .collect::<Result<Vec<_>, _>>()?;
    tokens_of_ids(&ids)
}
