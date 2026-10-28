//! The node's index of the token standards' events (D-120, `docs/STANDARDS.md`): the contracts that declared KS-20 or
//! KS-721 with their metadata, supply and number of holders, the balances of tokens and their holders by amount, the
//! owners of NFTs and the NFTs of each account, and approvals, kept from the receipts of the ops the main chain executes.
//! Local, not consensus: the VM runs these contracts as any other, and the index only reads what they emit.
//!
//! A contract's first `standards` event is its declaration (§2), and the events of its standard count from there on,
//! each only when its bytes are exactly one of the standard's events: the contract's other events, and every event of a
//! contract that declared nothing the index follows, are not the index's. An event of the standard that the index cannot
//! apply as the standard has it (a transfer of more than the sender holds by the token's own events, of an NFT `from`
//! does not own, a second declaration, metadata out of bounds) is passed over and marks the token irregular: its
//! balances may then differ from what the contract answers.
//!
//! The index lives in the block store's column family `standards` ([`Kv`] gives its reads and writes), written in the
//! batches that connect the main chain and roll it back, with an undo log by height (`crate::store`):
//!
//! | Prefix | Key | Value |
//! | --- | --- | --- |
//! | `0x01` | contract | Borsh([`Declared`]) |
//! | `0x02` | account ‖ contract | Borsh(`u128`): the account's balance of a token, or the number of a collection's NFTs it holds, above 0 |
//! | `0x03` | contract ‖ amount (16 bytes big-endian) ‖ account | empty: the holders by amount |
//! | `0x04` | contract ‖ token ID | the NFT's owner, the zero account once burned |
//! | `0x05` | account ‖ contract ‖ token ID | empty: the NFTs an account holds |
//! | `0x06` | owner ‖ contract ‖ kind (1 byte) ‖ ID | an approval: kind 0 a KS-20 allowance to the spender ID, Borsh(`(u128, Option<u64>)`); kind 1 a KS-721 operator ID, Borsh(`Option<u64>`); kind 2 the approved account (32 bytes) of the token ID |

use std::collections::BTreeSet;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_execution::Event;

use crate::NodeError;

/// The zero account: the `from` of a mint and the `to` of a burn, never a holder.
pub const ZERO_ACCOUNT: [u8; 32] = [0; 32];
/// The ID of KS-20.
pub const KS20: &str = "KS-20";
/// The ID of KS-721.
pub const KS721: &str = "KS-721";
/// The most IDs a declaration the index keeps holds.
pub const MAX_DECLARED_IDS: usize = 16;
/// The longest ID a declaration the index keeps holds, in bytes.
pub const MAX_ID_BYTES: usize = 64;
/// The longest name of a token or a collection, in bytes (`docs/STANDARDS.md` §3.2).
pub const MAX_NAME_BYTES: usize = 64;
/// The longest symbol, in bytes.
pub const MAX_SYMBOL_BYTES: usize = 16;
/// The most decimals of a KS-20 token.
pub const MAX_DECIMALS: u8 = 18;

const TOKEN: u8 = 0x01;
const BALANCE: u8 = 0x02;
const HOLDER: u8 = 0x03;
const OWNER: u8 = 0x04;
const OWNED: u8 = 0x05;
const APPROVAL: u8 = 0x06;

/// The kind of an approval in its key: a KS-20 allowance.
pub const ALLOWANCE: u8 = 0;
/// A KS-721 operator.
pub const OPERATOR: u8 = 1;
/// The approved account of one KS-721 token.
pub const TOKEN_APPROVAL: u8 = 2;

/// A standard the index follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Standard {
    /// Fungible tokens.
    Ks20,
    /// NFTs.
    Ks721,
}

impl Standard {
    /// The standard's ID.
    pub fn id(self) -> &'static str {
        match self {
            Standard::Ks20 => KS20,
            Standard::Ks721 => KS721,
        }
    }

    /// The standard a declaration names, when the index follows it: KS-20 or KS-721 first, and the other nowhere
    /// (a contract must not declare both).
    pub fn of(ids: &[String]) -> Option<Standard> {
        let (standard, other) = match ids.first()?.as_str() {
            KS20 => (Standard::Ks20, KS721),
            KS721 => (Standard::Ks721, KS20),
            _ => return None,
        };
        (!ids.iter().any(|id| id == other)).then_some(standard)
    }
}

/// A token's or a collection's metadata, from its latest `metadata` event the index applied.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Metadata {
    /// Name, 1 to 64 bytes.
    pub name: String,
    /// Symbol, 1 to 16 bytes.
    pub symbol: String,
    /// A KS-20 token's decimals; `None` for a collection.
    pub decimals: Option<u8>,
}

/// What the index keeps of a contract that made its declaration.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Declared {
    /// The position of the op whose receipt holds the declaration: height, slot and entry (`ksana_rpc::OpPosition`).
    pub at: (u64, u32, u32),
    /// The IDs declared, the standard's first; empty for a declaration that is not 1 to [`MAX_DECLARED_IDS`] IDs of 1
    /// to [`MAX_ID_BYTES`] bytes.
    pub ids: Vec<String>,
    /// The standard the index follows the contract by; `None` when the declaration names neither KS-20 nor KS-721
    /// first, or both.
    pub standard: Option<Standard>,
    /// The metadata, once a `metadata` event set it.
    pub metadata: Option<Metadata>,
    /// The sum of the balances (KS-20), or the number of NFTs in existence (KS-721).
    pub supply: u128,
    /// The number of accounts holding some.
    pub holders: u64,
    /// Whether the index passed over an event of the standard the contract emitted.
    pub irregular: bool,
}

/// A KS-20 event (`docs/STANDARDS.md` §3.4, §3.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ks20Event {
    /// `("standards", ids)`.
    Standards(Vec<String>),
    /// `("metadata", name, symbol, decimals)`.
    Metadata {
        /// Name.
        name: String,
        /// Symbol.
        symbol: String,
        /// Decimals.
        decimals: u8,
    },
    /// `("transfer", from, to, amount)`.
    Transfer {
        /// The account debited, the zero account for a mint.
        from: [u8; 32],
        /// The account credited, the zero account for a burn.
        to: [u8; 32],
        /// The amount.
        amount: u128,
    },
    /// `("approve", owner, spender, amount, expires_at)`.
    Approve {
        /// The account whose tokens the spender may take.
        owner: [u8; 32],
        /// The spender.
        spender: [u8; 32],
        /// The allowance set; 0 revokes it.
        amount: u128,
        /// When it stops counting; `None` never.
        expires_at: Option<u64>,
    },
    /// `("minter", previous, minter)` (`KS-20-mintable`).
    Minter {
        /// The minter before.
        previous: Option<[u8; 32]>,
        /// The minter now.
        minter: Option<[u8; 32]>,
    },
}

/// A KS-721 event (`docs/STANDARDS.md` §4.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ks721Event {
    /// `("standards", ids)`.
    Standards(Vec<String>),
    /// `("metadata", name, symbol)`.
    Metadata {
        /// Name.
        name: String,
        /// Symbol.
        symbol: String,
    },
    /// `("transfer", from, to, token_id)`.
    Transfer {
        /// The owner before, the zero account for a mint.
        from: [u8; 32],
        /// The owner now, the zero account for a burn.
        to: [u8; 32],
        /// The token.
        token_id: [u8; 32],
    },
    /// `("approve", owner, spender, token_id)`.
    Approve {
        /// The token's owner.
        owner: [u8; 32],
        /// The approved account; `None` clears it.
        spender: Option<[u8; 32]>,
        /// The token.
        token_id: [u8; 32],
    },
    /// `("operator", owner, operator, approved, expires_at)`.
    Operator {
        /// The owner whose tokens the operator may move.
        owner: [u8; 32],
        /// The operator.
        operator: [u8; 32],
        /// Whether it is an operator now; `false` takes it back.
        approved: bool,
        /// When it stops counting; `None` never.
        expires_at: Option<u64>,
    },
    /// `("uri", token_id)`: one token's URI changed, or every token's (`None`).
    Uri(Option<[u8; 32]>),
}

/// An approval of the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Approval {
    /// A KS-20 allowance, as the latest `approve` event set it.
    Allowance {
        /// The token.
        contract: [u8; 32],
        /// The spender.
        spender: [u8; 32],
        /// The amount set (what is left is the token's `allowance`).
        amount: u128,
        /// When it stops counting.
        expires_at: Option<u64>,
    },
    /// A KS-721 operator.
    Operator {
        /// The collection.
        contract: [u8; 32],
        /// The operator.
        operator: [u8; 32],
        /// When it stops counting.
        expires_at: Option<u64>,
    },
    /// The approved account of one NFT.
    Token {
        /// The collection.
        contract: [u8; 32],
        /// The token.
        token_id: [u8; 32],
        /// The approved account.
        spender: [u8; 32],
    },
}

/// The reads and writes of the index: the values of keys of the column family `standards`, `None` for an absent key.
pub trait Kv {
    /// The value of `key`.
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, NodeError>;
    /// Sets the value of `key` (`None` deletes it).
    fn set(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) -> Result<(), NodeError>;
}

fn decode<T: BorshDeserialize>(bytes: &[u8]) -> Result<T, NodeError> {
    borsh::from_slice(bytes)
        .map_err(|e| NodeError::Storage(format!("the standards index is corrupt: {e}")))
}

fn encode<T: BorshSerialize>(value: &T) -> Result<Vec<u8>, NodeError> {
    borsh::to_vec(value).map_err(|e| NodeError::Internal(e.to_string()))
}

fn account_of(bytes: &[u8]) -> Result<[u8; 32], NodeError> {
    bytes.try_into().map_err(|_| {
        NodeError::Storage("the standards index holds an account of another length".into())
    })
}

fn key(prefix: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut key = vec![prefix];
    for part in parts {
        key.extend_from_slice(part);
    }
    key
}

/// The key of a declared contract's record.
pub fn token_key(contract: &[u8; 32]) -> Vec<u8> {
    key(TOKEN, &[contract])
}

/// The key of an account's amount of a token or a collection.
pub fn balance_key(account: &[u8; 32], contract: &[u8; 32]) -> Vec<u8> {
    key(BALANCE, &[account, contract])
}

/// The prefix of an account's amounts, followed by the contract.
pub fn balances_prefix(account: &[u8; 32]) -> Vec<u8> {
    key(BALANCE, &[account])
}

fn holder_key(contract: &[u8; 32], amount: u128, account: &[u8; 32]) -> Vec<u8> {
    key(HOLDER, &[contract, &amount.to_be_bytes(), account])
}

/// The prefix of a contract's holders, followed by the amount (16 bytes big-endian) and the account.
pub fn holders_prefix(contract: &[u8; 32]) -> Vec<u8> {
    key(HOLDER, &[contract])
}

fn owner_key(contract: &[u8; 32], token_id: &[u8; 32]) -> Vec<u8> {
    key(OWNER, &[contract, token_id])
}

fn owned_key(account: &[u8; 32], contract: &[u8; 32], token_id: &[u8; 32]) -> Vec<u8> {
    key(OWNED, &[account, contract, token_id])
}

/// The prefix of the NFTs an account holds, followed by the contract and the token ID.
pub fn owned_prefix(account: &[u8; 32]) -> Vec<u8> {
    key(OWNED, &[account])
}

fn approval_key(owner: &[u8; 32], contract: &[u8; 32], kind: u8, id: &[u8; 32]) -> Vec<u8> {
    key(APPROVAL, &[owner, contract, &[kind], id])
}

/// The prefix of an owner's approvals, followed by the contract, the kind and the ID.
pub fn approvals_prefix(owner: &[u8; 32]) -> Vec<u8> {
    key(APPROVAL, &[owner])
}

/// A contract's record, from its stored value.
pub fn declared_of(value: &[u8]) -> Result<Declared, NodeError> {
    decode(value)
}

/// An amount, from its stored value.
pub fn amount_from(value: &[u8]) -> Result<u128, NodeError> {
    decode(value)
}

/// A holder, from the part of its key after the contract: the amount and the account.
pub fn holder_from(suffix: &[u8]) -> Result<(u128, [u8; 32]), NodeError> {
    let corrupt = || NodeError::Storage("a holder's key in the standards index is corrupt".into());
    let (amount, account) = suffix.split_first_chunk::<16>().ok_or_else(corrupt)?;
    Ok((u128::from_be_bytes(*amount), account_of(account)?))
}

/// An NFT, from the part of its key after the account: the contract and the token ID.
pub fn nft_from(suffix: &[u8]) -> Result<([u8; 32], [u8; 32]), NodeError> {
    let corrupt = || NodeError::Storage("an NFT's key in the standards index is corrupt".into());
    let (contract, token_id) = suffix.split_first_chunk::<32>().ok_or_else(corrupt)?;
    Ok((*contract, token_id.try_into().map_err(|_| corrupt())?))
}

/// An approval, from the part of its key after the owner (the contract, the kind and the ID) and its value.
pub fn approval_from(suffix: &[u8], value: &[u8]) -> Result<Approval, NodeError> {
    let corrupt = || NodeError::Storage("an approval in the standards index is corrupt".into());
    let (contract, rest) = suffix.split_first_chunk::<32>().ok_or_else(corrupt)?;
    let (kind, id) = rest.split_first().ok_or_else(corrupt)?;
    let id: [u8; 32] = id.try_into().map_err(|_| corrupt())?;
    let contract = *contract;
    Ok(match *kind {
        ALLOWANCE => {
            let (amount, expires_at) = decode(value)?;
            Approval::Allowance {
                contract,
                spender: id,
                amount,
                expires_at,
            }
        }
        OPERATOR => Approval::Operator {
            contract,
            operator: id,
            expires_at: decode(value)?,
        },
        TOKEN_APPROVAL => Approval::Token {
            contract,
            token_id: id,
            spender: account_of(value)?,
        },
        _ => return Err(corrupt()),
    })
}

/// An event's name and the bytes after it.
fn named(data: &[u8]) -> Option<(String, &[u8])> {
    let mut input = data;
    let name = String::deserialize(&mut input).ok()?;
    Some((name, input))
}

/// `T` decoded from all of `input`; `None` when it does not decode or bytes are left.
fn exact<T: BorshDeserialize>(input: &[u8]) -> Option<T> {
    let mut rest = input;
    let value = T::deserialize(&mut rest).ok()?;
    rest.is_empty().then_some(value)
}

/// The IDs of a declaration, `("standards", ids)`; `None` for any other bytes.
pub fn declaration(data: &[u8]) -> Option<Vec<String>> {
    let (name, rest) = named(data)?;
    if name == "standards" {
        exact(rest)
    } else {
        None
    }
}

impl Ks20Event {
    /// The event of `data`; `None` for another event or any other layout (the name and exactly its fields).
    pub fn decode(data: &[u8]) -> Option<Self> {
        let (name, rest) = named(data)?;
        Some(match name.as_str() {
            "standards" => Self::Standards(exact(rest)?),
            "metadata" => {
                let (name, symbol, decimals) = exact(rest)?;
                Self::Metadata {
                    name,
                    symbol,
                    decimals,
                }
            }
            "transfer" => {
                let (from, to, amount) = exact(rest)?;
                Self::Transfer { from, to, amount }
            }
            "approve" => {
                let (owner, spender, amount, expires_at) = exact(rest)?;
                Self::Approve {
                    owner,
                    spender,
                    amount,
                    expires_at,
                }
            }
            "minter" => {
                let (previous, minter) = exact(rest)?;
                Self::Minter { previous, minter }
            }
            _ => return None,
        })
    }
}

impl Ks721Event {
    /// The event of `data`; `None` for another event or any other layout.
    pub fn decode(data: &[u8]) -> Option<Self> {
        let (name, rest) = named(data)?;
        Some(match name.as_str() {
            "standards" => Self::Standards(exact(rest)?),
            "metadata" => {
                let (name, symbol) = exact(rest)?;
                Self::Metadata { name, symbol }
            }
            "transfer" => {
                let (from, to, token_id) = exact(rest)?;
                Self::Transfer { from, to, token_id }
            }
            "approve" => {
                let (owner, spender, token_id) = exact(rest)?;
                Self::Approve {
                    owner,
                    spender,
                    token_id,
                }
            }
            "operator" => {
                let (owner, operator, approved, expires_at) = exact(rest)?;
                Self::Operator {
                    owner,
                    operator,
                    approved,
                    expires_at,
                }
            }
            "uri" => Self::Uri(exact(rest)?),
            _ => return None,
        })
    }
}

/// Applies the events of an op that succeeded at position `at` (height, slot, entry) to the index, in the order the op
/// emitted them. Returns the accounts the standard events the index applied name (the zero account left out), with the
/// contracts that emitted them: the operation index lists the op under each (D-111).
pub fn apply(
    kv: &mut impl Kv,
    at: (u64, u32, u32),
    events: &[Event],
) -> Result<BTreeSet<[u8; 32]>, NodeError> {
    let mut named = BTreeSet::new();
    for event in events {
        let contract = event.contract;
        let key = token_key(&contract);
        let Some(before) = kv.get(&key)?.map(|b| declared_of(&b)).transpose()? else {
            if let Some(ids) = declaration(&event.data) {
                let kept = (1..=MAX_DECLARED_IDS).contains(&ids.len())
                    && ids.iter().all(|id| (1..=MAX_ID_BYTES).contains(&id.len()));
                let ids = if kept { ids } else { Vec::new() };
                let declared = Declared {
                    at,
                    standard: Standard::of(&ids),
                    ids,
                    metadata: None,
                    supply: 0,
                    holders: 0,
                    irregular: false,
                };
                kv.set(key, Some(encode(&declared)?))?;
                named.insert(contract);
            }
            continue;
        };
        let mut declared = before.clone();
        let mut accounts = Vec::new();
        let applied = match declared.standard {
            None => continue,
            Some(Standard::Ks20) => match Ks20Event::decode(&event.data) {
                Some(event) => ks20(kv, &contract, &mut declared, event, &mut accounts)?,
                None => continue,
            },
            Some(Standard::Ks721) => match Ks721Event::decode(&event.data) {
                Some(event) => ks721(kv, &contract, &mut declared, event, &mut accounts)?,
                None => continue,
            },
        };
        if applied {
            named.insert(contract);
            named.extend(accounts.into_iter().filter(|a| *a != ZERO_ACCOUNT));
        } else {
            declared.irregular = true;
        }
        if declared != before {
            kv.set(key, Some(encode(&declared)?))?;
        }
    }
    Ok(named)
}

fn fits(name: &str, symbol: &str) -> bool {
    (1..=MAX_NAME_BYTES).contains(&name.len()) && (1..=MAX_SYMBOL_BYTES).contains(&symbol.len())
}

fn amount_of(kv: &mut impl Kv, account: &[u8; 32], contract: &[u8; 32]) -> Result<u128, NodeError> {
    Ok(kv
        .get(&balance_key(account, contract))?
        .map(|b| amount_from(&b))
        .transpose()?
        .unwrap_or(0))
}

/// Sets `account`'s amount of `contract` to `amount`, with its place among the holders and their number.
fn set_amount(
    kv: &mut impl Kv,
    contract: &[u8; 32],
    account: &[u8; 32],
    amount: u128,
    declared: &mut Declared,
) -> Result<(), NodeError> {
    let old = amount_of(kv, account, contract)?;
    if old == amount {
        return Ok(());
    }
    if old > 0 {
        kv.set(holder_key(contract, old, account), None)?;
    }
    if amount > 0 {
        kv.set(holder_key(contract, amount, account), Some(Vec::new()))?;
        kv.set(balance_key(account, contract), Some(encode(&amount)?))?;
    } else {
        kv.set(balance_key(account, contract), None)?;
    }
    if old == 0 {
        declared.holders = declared.holders.saturating_add(1);
    } else if amount == 0 {
        declared.holders = declared.holders.saturating_sub(1);
    }
    Ok(())
}

/// Applies a KS-20 event; `false` when the index passes it over. `named` gets the accounts it names.
fn ks20(
    kv: &mut impl Kv,
    contract: &[u8; 32],
    declared: &mut Declared,
    event: Ks20Event,
    named: &mut Vec<[u8; 32]>,
) -> Result<bool, NodeError> {
    match event {
        Ks20Event::Standards(_) => Ok(false),
        Ks20Event::Metadata {
            name,
            symbol,
            decimals,
        } => {
            // The decimals never change (§3.2 rule 2)
            let same_decimals = declared
                .metadata
                .as_ref()
                .is_none_or(|m| m.decimals == Some(decimals));
            if !fits(&name, &symbol) || decimals > MAX_DECIMALS || !same_decimals {
                return Ok(false);
            }
            declared.metadata = Some(Metadata {
                name,
                symbol,
                decimals: Some(decimals),
            });
            Ok(true)
        }
        Ks20Event::Transfer { from, to, amount } => {
            if amount == 0 || (from == ZERO_ACCOUNT && to == ZERO_ACCOUNT) {
                return Ok(false);
            }
            // Every check first: an event passed over changes nothing
            let debited = if from == ZERO_ACCOUNT {
                None
            } else {
                let Some(left) = amount_of(kv, &from, contract)?.checked_sub(amount) else {
                    return Ok(false);
                };
                Some(left)
            };
            if to != ZERO_ACCOUNT
                && to != from
                && amount_of(kv, &to, contract)?.checked_add(amount).is_none()
            {
                return Ok(false);
            }
            let supply = if from == ZERO_ACCOUNT {
                declared.supply.checked_add(amount)
            } else if to == ZERO_ACCOUNT {
                declared.supply.checked_sub(amount)
            } else {
                Some(declared.supply)
            };
            let Some(supply) = supply else {
                return Ok(false);
            };
            if let Some(left) = debited {
                set_amount(kv, contract, &from, left, declared)?;
            }
            if to != ZERO_ACCOUNT {
                let credited = amount_of(kv, &to, contract)?.saturating_add(amount);
                set_amount(kv, contract, &to, credited, declared)?;
            }
            declared.supply = supply;
            named.extend([from, to]);
            Ok(true)
        }
        Ks20Event::Approve {
            owner,
            spender,
            amount,
            expires_at,
        } => {
            if owner == ZERO_ACCOUNT || spender == ZERO_ACCOUNT || owner == spender {
                return Ok(false);
            }
            let value = if amount > 0 {
                Some(encode(&(amount, expires_at))?)
            } else {
                None
            };
            kv.set(approval_key(&owner, contract, ALLOWANCE, &spender), value)?;
            named.extend([owner, spender]);
            Ok(true)
        }
        Ks20Event::Minter { previous, minter } => {
            named.extend(previous.into_iter().chain(minter));
            Ok(true)
        }
    }
}

/// Applies a KS-721 event; `false` when the index passes it over.
fn ks721(
    kv: &mut impl Kv,
    contract: &[u8; 32],
    declared: &mut Declared,
    event: Ks721Event,
    named: &mut Vec<[u8; 32]>,
) -> Result<bool, NodeError> {
    match event {
        Ks721Event::Standards(_) => Ok(false),
        Ks721Event::Metadata { name, symbol } => {
            if !fits(&name, &symbol) {
                return Ok(false);
            }
            declared.metadata = Some(Metadata {
                name,
                symbol,
                decimals: None,
            });
            Ok(true)
        }
        Ks721Event::Transfer { from, to, token_id } => {
            if from == ZERO_ACCOUNT && to == ZERO_ACCOUNT {
                return Ok(false);
            }
            let owner = kv
                .get(&owner_key(contract, &token_id))?
                .map(|b| account_of(&b))
                .transpose()?;
            let supply = if from == ZERO_ACCOUNT {
                // An ID is minted once: never again once it existed, burned or not (§4.2 rule 6)
                if owner.is_some() {
                    return Ok(false);
                }
                declared.supply.checked_add(1)
            } else {
                if owner != Some(from) || to == from {
                    return Ok(false);
                }
                if to == ZERO_ACCOUNT {
                    declared.supply.checked_sub(1)
                } else {
                    Some(declared.supply)
                }
            };
            let Some(supply) = supply else {
                return Ok(false);
            };
            if to != ZERO_ACCOUNT && amount_of(kv, &to, contract)?.checked_add(1).is_none() {
                return Ok(false);
            }
            kv.set(owner_key(contract, &token_id), Some(to.to_vec()))?;
            if from != ZERO_ACCOUNT {
                let left = amount_of(kv, &from, contract)?.saturating_sub(1);
                set_amount(kv, contract, &from, left, declared)?;
                kv.set(owned_key(&from, contract, &token_id), None)?;
                // A change of owner clears the token's approved account (§4.2 rule 3)
                kv.set(
                    approval_key(&from, contract, TOKEN_APPROVAL, &token_id),
                    None,
                )?;
            }
            if to != ZERO_ACCOUNT {
                let held = amount_of(kv, &to, contract)?.saturating_add(1);
                set_amount(kv, contract, &to, held, declared)?;
                kv.set(owned_key(&to, contract, &token_id), Some(Vec::new()))?;
            }
            declared.supply = supply;
            named.extend([from, to]);
            Ok(true)
        }
        Ks721Event::Approve {
            owner,
            spender,
            token_id,
        } => {
            let current = kv
                .get(&owner_key(contract, &token_id))?
                .map(|b| account_of(&b))
                .transpose()?;
            let refused = spender.is_some_and(|s| s == ZERO_ACCOUNT || s == owner);
            if owner == ZERO_ACCOUNT || current != Some(owner) || refused {
                return Ok(false);
            }
            kv.set(
                approval_key(&owner, contract, TOKEN_APPROVAL, &token_id),
                spender.map(|s| s.to_vec()),
            )?;
            named.extend(std::iter::once(owner).chain(spender));
            Ok(true)
        }
        Ks721Event::Operator {
            owner,
            operator,
            approved,
            expires_at,
        } => {
            if owner == ZERO_ACCOUNT || operator == ZERO_ACCOUNT || owner == operator {
                return Ok(false);
            }
            let value = if approved {
                Some(encode(&expires_at)?)
            } else {
                None
            };
            kv.set(approval_key(&owner, contract, OPERATOR, &operator), value)?;
            named.extend([owner, operator]);
            Ok(true)
        }
        Ks721Event::Uri(_) => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{json, Value};

    use super::*;

    /// The index in memory.
    #[derive(Default)]
    struct Mem(BTreeMap<Vec<u8>, Vec<u8>>);

    impl Kv for Mem {
        fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, NodeError> {
            Ok(self.0.get(key).cloned())
        }

        fn set(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) -> Result<(), NodeError> {
            match value {
                Some(value) => self.0.insert(key, value),
                None => self.0.remove(&key),
            };
            Ok(())
        }
    }

    fn vectors() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/vectors/standards.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn opt_hex(value: &Option<[u8; 32]>) -> Value {
        value.map_or(Value::Null, |v| json!(hex::encode(v)))
    }

    fn opt_dec(value: &Option<u64>) -> Value {
        value.map_or(Value::Null, |v| json!(v.to_string()))
    }

    /// An event as the vectors write it: the variant's name and its fields, in the D-007 JSON conventions.
    fn ks20_json(event: &Ks20Event) -> Value {
        match event {
            Ks20Event::Standards(ids) => json!({ "Standards": ids }),
            Ks20Event::Metadata {
                name,
                symbol,
                decimals,
            } => json!({ "Metadata": { "name": name, "symbol": symbol, "decimals": decimals } }),
            Ks20Event::Transfer { from, to, amount } => json!({ "Transfer": {
                "from": hex::encode(from), "to": hex::encode(to), "amount": amount.to_string() } }),
            Ks20Event::Approve {
                owner,
                spender,
                amount,
                expires_at,
            } => json!({ "Approve": {
                "owner": hex::encode(owner), "spender": hex::encode(spender), "amount": amount.to_string(),
                "expires_at": opt_dec(expires_at) } }),
            Ks20Event::Minter { previous, minter } => {
                json!({ "Minter": { "previous": opt_hex(previous), "minter": opt_hex(minter) } })
            }
        }
    }

    fn ks721_json(event: &Ks721Event) -> Value {
        match event {
            Ks721Event::Standards(ids) => json!({ "Standards": ids }),
            Ks721Event::Metadata { name, symbol } => {
                json!({ "Metadata": { "name": name, "symbol": symbol } })
            }
            Ks721Event::Transfer { from, to, token_id } => json!({ "Transfer": {
                "from": hex::encode(from), "to": hex::encode(to), "token_id": hex::encode(token_id) } }),
            Ks721Event::Approve {
                owner,
                spender,
                token_id,
            } => json!({ "Approve": {
                "owner": hex::encode(owner), "spender": opt_hex(spender), "token_id": hex::encode(token_id) } }),
            Ks721Event::Operator {
                owner,
                operator,
                approved,
                expires_at,
            } => json!({ "Operator": {
                "owner": hex::encode(owner), "operator": hex::encode(operator), "approved": approved,
                "expires_at": opt_dec(expires_at) } }),
            Ks721Event::Uri(token_id) => json!({ "Uri": opt_hex(token_id) }),
        }
    }

    fn bytes(vector: &Value) -> Vec<u8> {
        hex::decode(vector["borsh"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn events_decode_as_the_shared_vectors_have_them() {
        let vectors = vectors();
        let events = vectors["events"].as_array().unwrap();
        assert!(events.len() >= 20);
        for vector in events {
            let data = bytes(vector);
            let name = &vector["name"];
            let decoded = match vector["standard"].as_str().unwrap() {
                KS20 => ks20_json(&Ks20Event::decode(&data).unwrap()),
                KS721 => ks721_json(&Ks721Event::decode(&data).unwrap()),
                other => panic!("{other}"),
            };
            assert_eq!(decoded, vector["value"], "{name}");
            if let Some(ids) = vector["value"]["Standards"].as_array() {
                assert_eq!(declaration(&data).unwrap().len(), ids.len(), "{name}");
            }
        }
        for vector in vectors["invalid_events"].as_array().unwrap() {
            let data = bytes(vector);
            let name = &vector["name"];
            match vector["standard"].as_str().unwrap() {
                KS20 => assert_eq!(Ks20Event::decode(&data), None, "{name}"),
                KS721 => assert_eq!(Ks721Event::decode(&data), None, "{name}"),
                other => panic!("{other}"),
            }
        }
        // Each standard's transfer is no event of the other
        let transfer = |standard: &str| {
            events
                .iter()
                .find(|v| v["standard"] == standard && v["value"].get("Transfer").is_some())
                .map(bytes)
                .unwrap()
        };
        assert_eq!(Ks20Event::decode(&transfer(KS721)), None);
        assert_eq!(Ks721Event::decode(&transfer(KS20)), None);
    }

    #[test]
    fn a_declaration_names_one_standard_first() {
        let ids = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(Standard::of(&ids(&[KS20])), Some(Standard::Ks20));
        assert_eq!(
            Standard::of(&ids(&[KS721, "KS-721-burnable", "my-own"])),
            Some(Standard::Ks721)
        );
        assert_eq!(Standard::of(&ids(&[KS20, KS721])), None);
        assert_eq!(Standard::of(&ids(&["KS-20-burnable", KS20])), None);
        assert_eq!(Standard::of(&ids(&["ERC-20"])), None);
        assert_eq!(Standard::of(&[]), None);
    }

    const TOKEN_ID: [u8; 32] = [0xc0; 32];
    const T: [u8; 32] = [0x7a; 32];
    const N: [u8; 32] = [0x7b; 32];
    const A: [u8; 32] = [0xa1; 32];
    const B: [u8; 32] = [0xb2; 32];
    const C: [u8; 32] = [0xc3; 32];

    fn event(contract: [u8; 32], data: Vec<u8>) -> Event {
        Event { contract, data }
    }

    fn declare(contract: [u8; 32], ids: &[&str]) -> Event {
        event(
            contract,
            borsh::to_vec(&("standards", ids.to_vec())).unwrap(),
        )
    }

    fn transfer(from: [u8; 32], to: [u8; 32], amount: u128) -> Event {
        event(T, borsh::to_vec(&("transfer", from, to, amount)).unwrap())
    }

    fn nft(from: [u8; 32], to: [u8; 32], token_id: [u8; 32]) -> Event {
        event(N, borsh::to_vec(&("transfer", from, to, token_id)).unwrap())
    }

    fn record(kv: &mut Mem, contract: &[u8; 32]) -> Declared {
        declared_of(&kv.get(&token_key(contract)).unwrap().unwrap()).unwrap()
    }

    fn amount(kv: &mut Mem, account: &[u8; 32], contract: &[u8; 32]) -> u128 {
        amount_of(kv, account, contract).unwrap()
    }

    /// The holders of `contract` by amount, the largest first.
    fn holders(kv: &Mem, contract: &[u8; 32]) -> Vec<(u128, [u8; 32])> {
        let prefix = holders_prefix(contract);
        let mut out: Vec<(u128, [u8; 32])> =
            kv.0.keys()
                .filter_map(|k| k.strip_prefix(prefix.as_slice()))
                .map(|suffix| holder_from(suffix).unwrap())
                .collect();
        out.reverse();
        out
    }

    #[test]
    fn a_tokens_events_keep_balances_holders_and_supply() {
        let mut kv = Mem::default();
        // Events before the declaration are not the index's
        let named = apply(&mut kv, (1, 0, 0), &[transfer(ZERO_ACCOUNT, A, 5)]).unwrap();
        assert!(named.is_empty());
        assert!(kv.0.is_empty());
        let metadata = event(
            T,
            borsh::to_vec(&("metadata", "Ksana Ñame", "KSÑ", 8u8)).unwrap(),
        );
        let named = apply(
            &mut kv,
            (2, 0, 1),
            &[
                declare(T, &[KS20, "KS-20-burnable"]),
                metadata,
                transfer(ZERO_ACCOUNT, A, 1_000),
            ],
        )
        .unwrap();
        assert_eq!(named, BTreeSet::from([T, A]));
        let token = record(&mut kv, &T);
        assert_eq!(token.at, (2, 0, 1));
        assert_eq!(token.standard, Some(Standard::Ks20));
        assert_eq!(token.ids, [KS20, "KS-20-burnable"]);
        assert_eq!(
            token.metadata,
            Some(Metadata {
                name: "Ksana Ñame".into(),
                symbol: "KSÑ".into(),
                decimals: Some(8)
            })
        );
        assert_eq!(
            (token.supply, token.holders, token.irregular),
            (1_000, 1, false)
        );

        // A transfer with a fee (two events from the sender), then a burn
        apply(
            &mut kv,
            (3, 0, 0),
            &[transfer(A, B, 297), transfer(A, C, 3)],
        )
        .unwrap();
        assert_eq!(
            holders(&kv, &T),
            [(700, A), (297, B), (3, C)],
            "by amount, the largest first"
        );
        apply(&mut kv, (4, 0, 0), &[transfer(C, ZERO_ACCOUNT, 3)]).unwrap();
        assert_eq!(amount(&mut kv, &C, &T), 0);
        assert!(
            kv.get(&balance_key(&C, &T)).unwrap().is_none(),
            "no zero balance"
        );
        assert_eq!(holders(&kv, &T), [(700, A), (297, B)]);
        let token = record(&mut kv, &T);
        assert_eq!((token.supply, token.holders), (997, 2));
        // A transfer to oneself moves nothing
        apply(&mut kv, (5, 0, 0), &[transfer(B, B, 297)]).unwrap();
        assert_eq!(holders(&kv, &T), [(700, A), (297, B)]);

        // Passed over: more than the sender holds, zero, from and to zero, a second declaration, decimals changed,
        // a name too long; each leaves everything else as it was
        let kept = kv.0.clone();
        for passed in [
            transfer(B, A, 298),
            transfer(A, B, 0),
            transfer(ZERO_ACCOUNT, ZERO_ACCOUNT, 1),
            transfer(ZERO_ACCOUNT, A, u128::MAX),
            declare(T, &[KS20]),
            event(
                T,
                borsh::to_vec(&("metadata", "Ksana", "KSA", 6u8)).unwrap(),
            ),
            event(
                T,
                borsh::to_vec(&("metadata", "n".repeat(65), "KSA", 8u8)).unwrap(),
            ),
            event(
                T,
                borsh::to_vec(&("approve", A, A, 5u128, None::<u64>)).unwrap(),
            ),
        ] {
            let mut kv = Mem(kept.clone());
            let named = apply(&mut kv, (6, 0, 0), std::slice::from_ref(&passed)).unwrap();
            assert!(named.is_empty(), "{passed:?}");
            let token = record(&mut kv, &T);
            assert!(token.irregular, "{passed:?}");
            let mut regular = kv.0.clone();
            let mut expected = kept.clone();
            regular.remove(&token_key(&T));
            expected.remove(&token_key(&T));
            assert_eq!(regular, expected, "{passed:?}");
        }
        // Events of another layout, or another name, are not the standard's: nothing changes
        let mut kv = Mem(kept.clone());
        apply(
            &mut kv,
            (6, 0, 0),
            &[
                event(T, borsh::to_vec(&("transfer", A, B, TOKEN_ID)).unwrap()),
                event(T, borsh::to_vec(&("swap", A, 5u128)).unwrap()),
            ],
        )
        .unwrap();
        assert_eq!(kv.0, kept);
    }

    #[test]
    fn allowances_are_kept_as_set_and_zero_revokes() {
        let mut kv = Mem::default();
        apply(&mut kv, (1, 0, 0), &[declare(T, &[KS20])]).unwrap();
        let approve = |amount: u128, expires_at: Option<u64>| {
            event(
                T,
                borsh::to_vec(&("approve", A, B, amount, expires_at)).unwrap(),
            )
        };
        let named = apply(&mut kv, (2, 0, 0), &[approve(50, Some(1_767_225_600))]).unwrap();
        assert_eq!(named, BTreeSet::from([T, A, B]));
        let key = approval_key(&A, &T, ALLOWANCE, &B);
        let suffix = key
            .strip_prefix(approvals_prefix(&A).as_slice())
            .unwrap()
            .to_vec();
        assert_eq!(
            approval_from(&suffix, &kv.get(&key).unwrap().unwrap()).unwrap(),
            Approval::Allowance {
                contract: T,
                spender: B,
                amount: 50,
                expires_at: Some(1_767_225_600)
            }
        );
        apply(&mut kv, (3, 0, 0), &[approve(0, None)]).unwrap();
        assert_eq!(kv.get(&key).unwrap(), None);
        // The minter's events name both minters
        let minter = event(T, borsh::to_vec(&("minter", Some(A), Some(C))).unwrap());
        assert_eq!(
            apply(&mut kv, (4, 0, 0), &[minter]).unwrap(),
            BTreeSet::from([T, A, C])
        );
        assert!(!record(&mut kv, &T).irregular);
    }

    #[test]
    fn a_collections_events_keep_owners_counts_and_approvals() {
        let mut kv = Mem::default();
        let one = [[0u8; 16], 1u128.to_be_bytes()]
            .concat()
            .try_into()
            .unwrap();
        let two = [[0u8; 16], 2u128.to_be_bytes()]
            .concat()
            .try_into()
            .unwrap();
        apply(
            &mut kv,
            (1, 0, 0),
            &[
                declare(N, &[KS721, "KS-721-burnable"]),
                event(
                    N,
                    borsh::to_vec(&("metadata", "Ksana Ärt 🎨", "KÄRT")).unwrap(),
                ),
                nft(ZERO_ACCOUNT, A, one),
                nft(ZERO_ACCOUNT, A, two),
                nft(ZERO_ACCOUNT, B, TOKEN_ID),
            ],
        )
        .unwrap();
        let collection = record(&mut kv, &N);
        assert_eq!(collection.standard, Some(Standard::Ks721));
        assert_eq!(collection.metadata.as_ref().unwrap().decimals, None);
        assert_eq!((collection.supply, collection.holders), (3, 2));
        assert_eq!(holders(&kv, &N), [(2, A), (1, B)]);

        // An approval, cleared by the transfer; an operator
        let approve = event(N, borsh::to_vec(&("approve", A, Some(C), one)).unwrap());
        let operator = event(
            N,
            borsh::to_vec(&("operator", A, C, true, Some(9u64))).unwrap(),
        );
        apply(&mut kv, (2, 0, 0), &[approve, operator]).unwrap();
        let approved = approval_key(&A, &N, TOKEN_APPROVAL, &one);
        assert_eq!(kv.get(&approved).unwrap(), Some(C.to_vec()));
        let named = apply(&mut kv, (3, 0, 0), &[nft(A, B, one)]).unwrap();
        assert_eq!(named, BTreeSet::from([N, A, B]));
        assert_eq!(kv.get(&approved).unwrap(), None);
        assert!(kv.get(&owned_key(&A, &N, &one)).unwrap().is_none());
        assert!(kv.get(&owned_key(&B, &N, &one)).unwrap().is_some());
        assert_eq!(holders(&kv, &N), [(2, B), (1, A)]);
        let operator_key = approval_key(&A, &N, OPERATOR, &C);
        assert_eq!(
            decode::<Option<u64>>(&kv.get(&operator_key).unwrap().unwrap()).unwrap(),
            Some(9)
        );
        let revoke = event(
            N,
            borsh::to_vec(&("operator", A, C, false, None::<u64>)).unwrap(),
        );
        apply(&mut kv, (4, 0, 0), &[revoke]).unwrap();
        assert_eq!(kv.get(&operator_key).unwrap(), None);

        // A burn: the ID stays taken
        apply(&mut kv, (5, 0, 0), &[nft(B, ZERO_ACCOUNT, TOKEN_ID)]).unwrap();
        let collection = record(&mut kv, &N);
        assert_eq!((collection.supply, collection.holders), (2, 2));
        assert_eq!(
            kv.get(&owner_key(&N, &TOKEN_ID)).unwrap(),
            Some(ZERO_ACCOUNT.to_vec())
        );
        let kept = kv.0.clone();
        for passed in [
            nft(ZERO_ACCOUNT, A, TOKEN_ID),
            nft(ZERO_ACCOUNT, C, one),
            nft(A, C, one),
            nft(B, B, one),
            nft(C, A, [0x01; 32]),
            event(N, borsh::to_vec(&("approve", A, Some(C), one)).unwrap()),
            event(N, borsh::to_vec(&("approve", B, Some(B), one)).unwrap()),
            event(
                N,
                borsh::to_vec(&("operator", A, A, true, None::<u64>)).unwrap(),
            ),
        ] {
            let mut kv = Mem(kept.clone());
            assert!(apply(&mut kv, (6, 0, 0), std::slice::from_ref(&passed))
                .unwrap()
                .is_empty());
            assert!(record(&mut kv, &N).irregular, "{passed:?}");
        }
    }

    #[test]
    fn declarations_the_index_does_not_follow_are_kept_but_not_indexed() {
        let mut kv = Mem::default();
        let too_many: Vec<String> = (0..=MAX_DECLARED_IDS).map(|i| format!("X-{i}")).collect();
        let long = "x".repeat(MAX_ID_BYTES.checked_add(1).unwrap());
        apply(
            &mut kv,
            (1, 2, 3),
            &[
                declare(T, &[KS20, KS721]),
                declare(N, &["ERC-721"]),
                event(A, borsh::to_vec(&("standards", too_many)).unwrap()),
                event(
                    B,
                    borsh::to_vec(&("standards", vec![KS20.to_string(), long])).unwrap(),
                ),
                transfer(ZERO_ACCOUNT, A, 5),
            ],
        )
        .unwrap();
        for (contract, ids) in [
            (T, vec![KS20, KS721]),
            (N, vec!["ERC-721"]),
            (A, vec![]),
            (B, vec![]),
        ] {
            let declared = record(&mut kv, &contract);
            assert_eq!(declared.ids, ids);
            assert_eq!(declared.standard, None);
            assert_eq!(declared.at, (1, 2, 3));
        }
        assert_eq!(amount(&mut kv, &A, &T), 0);
        // A later declaration changes nothing
        apply(&mut kv, (2, 0, 0), &[declare(N, &[KS721])]).unwrap();
        assert_eq!(record(&mut kv, &N).standard, None);
    }
}
