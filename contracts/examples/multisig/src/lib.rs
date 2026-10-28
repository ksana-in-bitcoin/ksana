//! Example contract: an M-of-N multisignature wallet.
//!
//! The wallet has a set of owners (user accounts or contracts, 1 to [`MAX_OWNERS`]) and a threshold, and holds
//! built-in assets: anyone can send it assets with the built-in `AssetTransfer` action or attach them to `deposit` (or
//! to the deployment). An owner proposes an [`Action`], owners approve it, and once the approvals of current owners
//! reach the threshold, any owner executes it. An action transfers an asset, calls another contract with funds from
//! the wallet attached, or changes the owners or the threshold: the wallet's settings change only through its own
//! proposals.
//!
//! Arguments and return values are Borsh-encoded. The arguments are a tuple in the order of the table below (a single
//! argument is just itself; no arguments is empty bytes):
//!
//! | Method | Arguments | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment; funds may be attached) | `(owners: Vec<[u8; 32]>, threshold: u8)` | `()` |
//! | `deposit` (funds attached) | none | `()` |
//! | `propose` | `action: Action` | `u64` (the proposal's ID) |
//! | `approve` | `id: u64` | `()` |
//! | `revoke` | `id: u64` | `()` |
//! | `execute` | `id: u64` | `Vec<u8>` (a `Call`'s output; empty for the other actions) |
//! | `cancel` | `id: u64` | `()` |
//! | `wallet` | none | [`Wallet`] |
//! | `proposal` | `id: u64` | `Option<Proposal>` |
//! | `proposals` | `(start: u64, limit: u32)` | `Vec<Proposal>` |
//! | `balance` | `asset: [u8; 32]` | `u128` (the wallet's balance of the asset) |
//!
//! Rules:
//!
//! - `init`: 1 to 20 owners, no duplicates, neither the zero account nor the wallet itself (which could never
//!   approve); `1 ≤ threshold ≤` the number of owners. Funds attached to the deployment stay in the wallet.
//! - `deposit`: anyone, with at least one asset attached; the assets stay in the wallet. `balance` also counts assets
//!   sent with `AssetTransfer`, which arrive without a call or an event.
//! - `propose`: owners only. The proposal starts `Pending` with the proposer's approval and the block timestamp as
//!   `created_at`; IDs start at 1. The action's shape is checked now, and the owner actions against the owners and the
//!   threshold of the moment (`execute` checks again against the state then):
//!   - `Transfer(asset, to, amount)`: `to` is not the zero account, `amount > 0`.
//!   - `Call(contract, method, args, funds)`: `contract` is neither the zero account nor the wallet itself (the owner
//!     actions change the wallet); `method` is 1 to [`MAX_METHOD_BYTES`] bytes and not `init`; `args` are at most
//!     [`MAX_ARGS_BYTES`] bytes; `funds` are at most 16 items, asset IDs strictly ascending, every amount above 0. These
//!     are the host's own rules for attached funds (ABI §2 item 7): checked here, a wrong list reverts with a clear
//!     reason at once instead of failing every execution.
//!   - `AddOwner(account)`: neither the zero account nor the wallet, not an owner, fewer than 20 owners.
//!   - `RemoveOwner(account)`: an owner, and the owners left still reach the threshold.
//!   - `SetThreshold(threshold)`: `1 ≤ threshold ≤` the number of owners.
//! - `approve`: owners only, on a pending proposal they have not approved. `revoke`: owners only, takes back the
//!   caller's approval of a pending proposal.
//! - `execute`: any owner, on a pending proposal whose approvals reach the threshold. The action is checked again
//!   against the state of the moment: the owner rules, and the wallet's balance of a `Transfer`'s amount and of each of
//!   a `Call`'s funds (`insufficient balance`). The proposal is marked `Executed` before the action runs, so a called
//!   contract that calls back into the wallet cannot execute it twice. A `Call` whose callee fails (reverts, traps,
//!   runs out of gas or does not exist) fails the whole operation: nothing changes, and the proposal stays pending, to
//!   be executed again or cancelled.
//! - `cancel`: the proposer, while still an owner, on a pending proposal.
//! - An approval counts only while its owner stays an owner. Removing an owner drops its approvals lazily: nothing is
//!   rewritten at the removal, `execute` counts only the approvals of current owners, and every later write to a
//!   pending proposal leaves the void ones out. Each owner is kept with a term, 0 for the owners given to `init` and a
//!   new number for each owner a proposal adds, and an approval records the approver's term: an account removed and
//!   added back must approve again. A proposal holds at most 20 approvals; once executed or cancelled, it keeps the
//!   approvals that counted then.
//! - `proposals(start, limit)`: the proposals by ID from `start` (0 means 1), at most `limit` and at most
//!   [`MAX_PAGE_ITEMS`] of them, fewer when the next one would take the page past [`MAX_PAGE_BYTES`] (a `Call` carries
//!   up to 16 KiB of arguments, and a call returns at most 64 KiB). An empty page means there are no more.
//! - Only `init` and `deposit` take attached funds: every other method reverts with `unexpected attached funds`, so
//!   that nothing gets stuck in the wallet. Views only read.
//!
//! Events (Borsh): `("deposit", from, asset, amount)` for each attached asset (`from` is the deployer at the
//! deployment), `("propose", id, proposer)`, `("approve", id, owner)`, `("revoke", id, owner)`,
//! `("execute", id, executor)`, `("cancel", id)`, and for the settings, at `init` and when an owner action is executed,
//! `("owner_added", owner)`, `("owner_removed", owner)` and `("threshold", threshold)`.
//!
//! The ABI the contract playground reads:
//!
//! ```abi
//! struct Funds { asset: AssetId, amount: u128 }
//! enum Action { Transfer(AssetId, Account, u128), Call(ContractId, String, Bytes, Vec<Funds>), AddOwner(Account), RemoveOwner(Account), SetThreshold(u8) }
//! enum Status { Pending, Executed, Cancelled }
//! struct Proposal { id: u64, proposer: Account, action: Action, approvals: Vec<Account>, status: Status, created_at: u64 }
//! struct Wallet { owners: Vec<Account>, threshold: u8, proposals: u64 }
//! payable init(owners: Vec<Account>, threshold: u8)          // 1 to 20 owners; attached funds go into the wallet
//! payable call deposit()                                     // attach any assets; anyone may deposit
//! call propose(action: Action) -> u64                        // owner only; counts as the proposer's approval; returns the ID
//! call approve(id: u64)                                      // owner only
//! call revoke(id: u64)                                       // takes back the caller's approval
//! call execute(id: u64) -> Bytes                             // owner only, once approvals reach the threshold; returns a Call's output
//! call cancel(id: u64)                                       // the proposer only, while pending
//! view wallet() -> Wallet
//! view proposal(id: u64) -> Option<Proposal>
//! view proposals(start: u64, limit: u32) -> Vec<Proposal>    // by ID from start (start 0 means 1), at most 50
//! view balance(asset: AssetId) -> u128                       // the wallet's balance
//! event deposit(from: Account, asset: AssetId, amount: u128)
//! event propose(id: u64, proposer: Account)
//! event approve(id: u64, owner: Account)
//! event revoke(id: u64, owner: Account)
//! event execute(id: u64, executor: Account)
//! event cancel(id: u64)
//! event owner_added(owner: Account)
//! event owner_removed(owner: Account)
//! event threshold(threshold: u8)
//! ```

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::borsh::BorshSerialize;
use ksana_contract_sdk::{self as sdk, storage, CallResult, Decode, Funds};

sdk::entry!(dispatch);

/// Most owners a wallet has.
pub const MAX_OWNERS: usize = 20;
/// Longest method name a `Call` action names, in bytes.
pub const MAX_METHOD_BYTES: usize = 64;
/// Longest arguments a `Call` action passes, in bytes (16 KiB).
pub const MAX_ARGS_BYTES: usize = 16_384;
/// Most proposals one page of `proposals` returns.
pub const MAX_PAGE_ITEMS: u32 = 50;
/// Most bytes the proposals of one page of `proposals` take, so that the page fits in the 64 KiB a call returns. A
/// proposal takes at most about 18 000 bytes (a `Call` with the longest arguments), so a page holds at least three.
pub const MAX_PAGE_BYTES: usize = 64_000;

/// The settings: [`Config`].
const KEY_CONFIG: &[u8] = b"config";
/// `p ‖ id` (u64 big-endian): what changes in a proposal, [`Record`].
const PREFIX_RECORD: &[u8] = b"p";
/// `a ‖ id` (u64 big-endian): the [`Action`] of a proposal, written once.
const PREFIX_ACTION: &[u8] = b"a";

const ZERO_ACCOUNT: [u8; 32] = [0u8; 32];

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_UNEXPECTED_FUNDS: &[u8] = b"unexpected attached funds";
const ERR_NOTHING_ATTACHED: &[u8] = b"attach the assets to deposit";
const ERR_OWNER_COUNT: &[u8] = b"owners must be 1 to 20";
const ERR_DUPLICATE_OWNER: &[u8] = b"duplicate owner";
const ERR_ZERO_OWNER: &[u8] = b"an owner must not be the zero account";
const ERR_SELF_OWNER: &[u8] = b"the wallet cannot own itself";
const ERR_THRESHOLD: &[u8] = b"threshold must be 1 to the number of owners";
const ERR_NOT_OWNER: &[u8] = b"caller is not an owner";
const ERR_UNKNOWN_PROPOSAL: &[u8] = b"unknown proposal";
const ERR_NOT_PENDING: &[u8] = b"proposal is not pending";
const ERR_ALREADY_APPROVED: &[u8] = b"already approved";
const ERR_NOT_APPROVED: &[u8] = b"not approved by the caller";
const ERR_NOT_ENOUGH_APPROVALS: &[u8] = b"not enough approvals";
const ERR_INSUFFICIENT_BALANCE: &[u8] = b"insufficient balance";
const ERR_NOT_PROPOSER: &[u8] = b"only the proposer can cancel";
const ERR_ZERO_RECIPIENT: &[u8] = b"recipient must not be the zero account";
const ERR_ZERO_AMOUNT: &[u8] = b"amount must be positive";
const ERR_SELF_CALL: &[u8] = b"use the owner actions to change the wallet";
const ERR_ZERO_CONTRACT: &[u8] = b"contract must not be the zero account";
const ERR_METHOD: &[u8] = b"method must be 1 to 64 bytes and not init";
const ERR_ARGS_TOO_LONG: &[u8] = b"args must be at most 16 KiB";
const ERR_TOO_MANY_FUNDS: &[u8] = b"at most 16 funds";
const ERR_ZERO_FUNDS: &[u8] = b"funds amounts must be positive";
const ERR_FUNDS_ORDER: &[u8] = b"funds must be sorted by asset without duplicates";
const ERR_ALREADY_OWNER: &[u8] = b"already an owner";
const ERR_TOO_MANY_OWNERS: &[u8] = b"at most 20 owners";
const ERR_UNKNOWN_OWNER: &[u8] = b"account is not an owner";
const ERR_BELOW_THRESHOLD: &[u8] = b"removal would leave fewer owners than the threshold";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";

/// What a proposal does once executed (Borsh: a `u8` variant index, then the fields in order).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub enum Action {
    /// `(asset, to, amount)`: sends `amount` of the built-in asset `asset` from the wallet to `to`.
    Transfer([u8; 32], [u8; 32], u128),
    /// `(contract, method, args, funds)`: calls `method` of `contract` with the argument bytes `args`, attaching
    /// `funds` from the wallet; the callee's output is what `execute` returns.
    Call([u8; 32], String, Vec<u8>, Vec<Funds>),
    /// Adds an owner.
    AddOwner([u8; 32]),
    /// Removes an owner.
    RemoveOwner([u8; 32]),
    /// Sets the threshold.
    SetThreshold(u8),
}

impl Decode for Action {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(Action::Transfer(
                <[u8; 32]>::decode(input)?,
                <[u8; 32]>::decode(input)?,
                u128::decode(input)?,
            )),
            1 => Some(Action::Call(
                <[u8; 32]>::decode(input)?,
                String::decode(input)?,
                Vec::<u8>::decode(input)?,
                Vec::<Funds>::decode(input)?,
            )),
            2 => Some(Action::AddOwner(<[u8; 32]>::decode(input)?)),
            3 => Some(Action::RemoveOwner(<[u8; 32]>::decode(input)?)),
            4 => Some(Action::SetThreshold(u8::decode(input)?)),
            _ => None,
        }
    }
}

/// Where a proposal stands (Borsh: a `u8` variant index).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub enum Status {
    /// Waiting for approvals and execution.
    Pending,
    /// Executed: its action ran.
    Executed,
    /// Cancelled by its proposer.
    Cancelled,
}

impl Decode for Status {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(Status::Pending),
            1 => Some(Status::Executed),
            2 => Some(Status::Cancelled),
            _ => None,
        }
    }
}

/// A proposal, as the views return it (the Borsh field order is as listed).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Proposal {
    /// ID, from 1.
    pub id: u64,
    /// The owner that proposed it.
    pub proposer: [u8; 32],
    /// What it does.
    pub action: Action,
    /// The approvals that count, in the order given: of a pending proposal, those of current owners given in their
    /// current term; of an executed or cancelled one, those that counted at that moment.
    pub approvals: Vec<[u8; 32]>,
    /// Where it stands.
    pub status: Status,
    /// The block timestamp when it was proposed (Unix seconds).
    pub created_at: u64,
}

impl Decode for Proposal {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            id: u64::decode(input)?,
            proposer: <[u8; 32]>::decode(input)?,
            action: Action::decode(input)?,
            approvals: Vec::<[u8; 32]>::decode(input)?,
            status: Status::decode(input)?,
            created_at: u64::decode(input)?,
        })
    }
}

/// The wallet's settings, as `wallet` returns them (the Borsh field order is as listed).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Wallet {
    /// The owners, in the order they became owners.
    pub owners: Vec<[u8; 32]>,
    /// How many approvals of current owners a proposal needs to be executed.
    pub threshold: u8,
    /// How many proposals have been made: the latest ID.
    pub proposals: u64,
}

impl Decode for Wallet {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            owners: Vec::<[u8; 32]>::decode(input)?,
            threshold: u8::decode(input)?,
            proposals: u64::decode(input)?,
        })
    }
}

/// An owner in one term of ownership: the account and the term's number. The owners are kept as members, and an
/// approval is the member that gave it: it counts while that same member is among the owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
struct Member {
    account: [u8; 32],
    /// 0 for the owners given to `init`; the owners added by proposals are numbered from 1.
    term: u64,
}

impl Decode for Member {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            account: <[u8; 32]>::decode(input)?,
            term: u64::decode(input)?,
        })
    }
}

/// The settings, under [`KEY_CONFIG`].
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
struct Config {
    /// The owners, in the order they became owners.
    owners: Vec<Member>,
    threshold: u8,
    /// How many proposals have been made: the latest ID.
    proposals: u64,
    /// How many owners proposals have added: the latest term.
    terms: u64,
}

impl Decode for Config {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            owners: Vec::<Member>::decode(input)?,
            threshold: u8::decode(input)?,
            proposals: u64::decode(input)?,
            terms: u64::decode(input)?,
        })
    }
}

impl Config {
    /// The account's membership, when it is an owner.
    fn member(&self, account: &[u8; 32]) -> Option<Member> {
        self.owners.iter().find(|m| m.account == *account).copied()
    }

    /// The approvals that count: those whose member is still among the owners.
    fn counted(&self, approvals: &[Member]) -> Vec<Member> {
        approvals
            .iter()
            .filter(|a| self.owners.contains(a))
            .copied()
            .collect()
    }
}

/// What changes in a proposal, under [`PREFIX_RECORD`] and its ID; the action is kept apart, so that an approval does
/// not rewrite a `Call`'s arguments.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
struct Record {
    proposer: [u8; 32],
    /// The approvals, as members; while pending, some may have been voided by the removal of their owner.
    approvals: Vec<Member>,
    status: Status,
    created_at: u64,
}

impl Decode for Record {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            proposer: <[u8; 32]>::decode(input)?,
            approvals: Vec::<Member>::decode(input)?,
            status: Status::decode(input)?,
            created_at: u64::decode(input)?,
        })
    }
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let (owners, threshold) = sdk::decode(args)?;
            init(owners, threshold)?;
            sdk::ok(&())
        }
        "deposit" => {
            sdk::decode::<()>(args)?;
            deposit()?;
            sdk::ok(&())
        }
        "propose" => {
            let action = sdk::decode(args)?;
            sdk::ok(&propose(action)?)
        }
        "approve" => {
            let id = sdk::decode(args)?;
            approve(id)?;
            sdk::ok(&())
        }
        "revoke" => {
            let id = sdk::decode(args)?;
            revoke(id)?;
            sdk::ok(&())
        }
        "execute" => {
            let id = sdk::decode(args)?;
            sdk::ok(&execute(id)?)
        }
        "cancel" => {
            let id = sdk::decode(args)?;
            cancel(id)?;
            sdk::ok(&())
        }
        "wallet" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&wallet()?)
        }
        "proposal" => {
            let id = sdk::decode(args)?;
            sdk::ok(&proposal(id)?)
        }
        "proposals" => {
            let (start, limit) = sdk::decode(args)?;
            sdk::ok(&proposals(start, limit)?)
        }
        "balance" => {
            let asset = sdk::decode(args)?;
            sdk::ok(&balance(&asset)?)
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

fn record_key(id: u64) -> Vec<u8> {
    [PREFIX_RECORD, id.to_be_bytes().as_slice()].concat()
}

fn action_key(id: u64) -> Vec<u8> {
    [PREFIX_ACTION, id.to_be_bytes().as_slice()].concat()
}

/// Only `init` and `deposit` take attached funds: anything attached to another method would be stuck in the wallet.
fn no_funds() -> Result<(), Vec<u8>> {
    if sdk::call_funds().is_empty() {
        Ok(())
    } else {
        Err(err(ERR_UNEXPECTED_FUNDS))
    }
}

fn load_config() -> Result<Config, Vec<u8>> {
    storage::read(KEY_CONFIG)?.ok_or_else(|| err(ERR_NOT_INITIALIZED))
}

/// The settings and the caller's membership; reverts when the caller is not an owner.
fn as_owner() -> Result<(Config, Member), Vec<u8>> {
    let config = load_config()?;
    let caller = config
        .member(&sdk::caller())
        .ok_or_else(|| err(ERR_NOT_OWNER))?;
    Ok((config, caller))
}

fn load_record(id: u64) -> Result<Record, Vec<u8>> {
    storage::read(&record_key(id))?.ok_or_else(|| err(ERR_UNKNOWN_PROPOSAL))
}

/// The record of a pending proposal.
fn pending(id: u64) -> Result<Record, Vec<u8>> {
    let record = load_record(id)?;
    if record.status != Status::Pending {
        return Err(err(ERR_NOT_PENDING));
    }
    Ok(record)
}

fn load_action(id: u64) -> Result<Action, Vec<u8>> {
    storage::read(&action_key(id))?.ok_or_else(|| err(ERR_UNKNOWN_PROPOSAL))
}

/// An account that becomes an owner: neither the zero account nor the wallet itself, which could never approve.
fn check_new_owner(account: &[u8; 32]) -> Result<(), Vec<u8>> {
    if *account == ZERO_ACCOUNT {
        return Err(err(ERR_ZERO_OWNER));
    }
    if *account == sdk::self_id() {
        return Err(err(ERR_SELF_OWNER));
    }
    Ok(())
}

fn check_threshold(threshold: u8, owners: usize) -> Result<(), Vec<u8>> {
    if threshold == 0 || usize::from(threshold) > owners {
        return Err(err(ERR_THRESHOLD));
    }
    Ok(())
}

/// The host's rules for attached funds (ABI §2 item 7).
fn check_funds(funds: &[Funds]) -> Result<(), Vec<u8>> {
    if funds.len() > sdk::MAX_CALL_FUNDS {
        return Err(err(ERR_TOO_MANY_FUNDS));
    }
    if funds.iter().any(|f| f.amount == 0) {
        return Err(err(ERR_ZERO_FUNDS));
    }
    if !funds
        .windows(2)
        .all(|pair| matches!(pair, [a, b] if a.asset < b.asset))
    {
        return Err(err(ERR_FUNDS_ORDER));
    }
    Ok(())
}

/// Checks an action's shape, and an owner action against the current settings: at `propose` and again at `execute`.
fn check_action(action: &Action, config: &Config) -> Result<(), Vec<u8>> {
    match action {
        Action::Transfer(_, to, amount) => {
            if *to == ZERO_ACCOUNT {
                return Err(err(ERR_ZERO_RECIPIENT));
            }
            if *amount == 0 {
                return Err(err(ERR_ZERO_AMOUNT));
            }
        }
        Action::Call(contract, method, args, funds) => {
            if *contract == sdk::self_id() {
                return Err(err(ERR_SELF_CALL));
            }
            if *contract == ZERO_ACCOUNT {
                return Err(err(ERR_ZERO_CONTRACT));
            }
            if method.is_empty() || method.len() > MAX_METHOD_BYTES || method == "init" {
                return Err(err(ERR_METHOD));
            }
            if args.len() > MAX_ARGS_BYTES {
                return Err(err(ERR_ARGS_TOO_LONG));
            }
            check_funds(funds)?;
        }
        Action::AddOwner(account) => {
            check_new_owner(account)?;
            if config.member(account).is_some() {
                return Err(err(ERR_ALREADY_OWNER));
            }
            if config.owners.len() >= MAX_OWNERS {
                return Err(err(ERR_TOO_MANY_OWNERS));
            }
        }
        Action::RemoveOwner(account) => {
            if config.member(account).is_none() {
                return Err(err(ERR_UNKNOWN_OWNER));
            }
            let left = config.owners.len().checked_sub(1).ok_or_else(overflow)?;
            if left < usize::from(config.threshold) {
                return Err(err(ERR_BELOW_THRESHOLD));
            }
        }
        Action::SetThreshold(threshold) => check_threshold(*threshold, config.owners.len())?,
    }
    Ok(())
}

/// The wallet holds `amount` of `asset`.
fn check_covered(asset: &[u8; 32], amount: u128) -> Result<(), Vec<u8>> {
    if sdk::asset_balance(asset, &sdk::self_id()) < amount {
        return Err(err(ERR_INSUFFICIENT_BALANCE));
    }
    Ok(())
}

/// The settings after an owner action; `None` for the other actions.
fn settings_after(action: &Action, mut config: Config) -> Result<Option<Config>, Vec<u8>> {
    match action {
        Action::AddOwner(account) => {
            let term = config.terms.checked_add(1).ok_or_else(overflow)?;
            config.terms = term;
            config.owners.push(Member {
                account: *account,
                term,
            });
        }
        Action::RemoveOwner(account) => config.owners.retain(|m| m.account != *account),
        Action::SetThreshold(threshold) => config.threshold = *threshold,
        Action::Transfer(..) | Action::Call(..) => return Ok(None),
    }
    Ok(Some(config))
}

/// `init(owners, threshold)`: the attached funds stay in the wallet.
pub fn init(owners: Vec<[u8; 32]>, threshold: u8) -> Result<(), Vec<u8>> {
    if storage::get(KEY_CONFIG).is_some() {
        return Err(err(ERR_ALREADY_INITIALIZED));
    }
    if owners.is_empty() || owners.len() > MAX_OWNERS {
        return Err(err(ERR_OWNER_COUNT));
    }
    for (index, owner) in owners.iter().enumerate() {
        check_new_owner(owner)?;
        if owners.iter().take(index).any(|earlier| earlier == owner) {
            return Err(err(ERR_DUPLICATE_OWNER));
        }
    }
    check_threshold(threshold, owners.len())?;
    let config = Config {
        owners: owners
            .iter()
            .map(|&account| Member { account, term: 0 })
            .collect(),
        threshold,
        proposals: 0,
        terms: 0,
    };
    storage::write(KEY_CONFIG, &config);
    for owner in owners {
        sdk::emit(&("owner_added", owner));
    }
    sdk::emit(&("threshold", threshold));
    let deployer = sdk::caller();
    for funds in sdk::call_funds() {
        sdk::emit(&("deposit", deployer, funds.asset, funds.amount));
    }
    Ok(())
}

/// `deposit()`: the attached assets stay in the wallet.
pub fn deposit() -> Result<(), Vec<u8>> {
    load_config()?;
    let attached = sdk::call_funds();
    if attached.is_empty() {
        return Err(err(ERR_NOTHING_ATTACHED));
    }
    let from = sdk::caller();
    for funds in attached {
        sdk::emit(&("deposit", from, funds.asset, funds.amount));
    }
    Ok(())
}

/// `propose(action) -> id`: a pending proposal with the proposer's approval.
pub fn propose(action: Action) -> Result<u64, Vec<u8>> {
    no_funds()?;
    let (mut config, proposer) = as_owner()?;
    check_action(&action, &config)?;
    let id = config.proposals.checked_add(1).ok_or_else(overflow)?;
    config.proposals = id;
    storage::write(KEY_CONFIG, &config);
    storage::write(&action_key(id), &action);
    storage::write(
        &record_key(id),
        &Record {
            proposer: proposer.account,
            approvals: Vec::from([proposer]),
            status: Status::Pending,
            created_at: sdk::block_timestamp(),
        },
    );
    sdk::emit(&("propose", id, proposer.account));
    Ok(id)
}

/// `approve(id)`: adds the caller's approval.
pub fn approve(id: u64) -> Result<(), Vec<u8>> {
    no_funds()?;
    let (config, owner) = as_owner()?;
    let mut record = pending(id)?;
    let mut approvals = config.counted(&record.approvals);
    if approvals.contains(&owner) {
        return Err(err(ERR_ALREADY_APPROVED));
    }
    approvals.push(owner);
    record.approvals = approvals;
    storage::write(&record_key(id), &record);
    sdk::emit(&("approve", id, owner.account));
    Ok(())
}

/// `revoke(id)`: takes back the caller's approval.
pub fn revoke(id: u64) -> Result<(), Vec<u8>> {
    no_funds()?;
    let (config, owner) = as_owner()?;
    let mut record = pending(id)?;
    let approvals = config.counted(&record.approvals);
    if !approvals.contains(&owner) {
        return Err(err(ERR_NOT_APPROVED));
    }
    record.approvals = approvals.into_iter().filter(|a| *a != owner).collect();
    storage::write(&record_key(id), &record);
    sdk::emit(&("revoke", id, owner.account));
    Ok(())
}

/// `execute(id) -> output`: runs the action of a proposal whose approvals reach the threshold; returns a `Call`'s
/// output, empty bytes for the other actions.
pub fn execute(id: u64) -> Result<Vec<u8>, Vec<u8>> {
    no_funds()?;
    let (config, executor) = as_owner()?;
    let mut record = pending(id)?;
    let approvals = config.counted(&record.approvals);
    if approvals.len() < usize::from(config.threshold) {
        return Err(err(ERR_NOT_ENOUGH_APPROVALS));
    }
    let action = load_action(id)?;
    check_action(&action, &config)?;
    match &action {
        Action::Transfer(asset, _, amount) => check_covered(asset, *amount)?,
        Action::Call(_, _, _, funds) => {
            for f in funds {
                check_covered(&f.asset, f.amount)?;
            }
        }
        Action::AddOwner(_) | Action::RemoveOwner(_) | Action::SetThreshold(_) => {}
    }
    let settings = settings_after(&action, config)?;
    // Executed before the action runs: a called contract that calls back cannot execute it again
    record.approvals = approvals;
    record.status = Status::Executed;
    storage::write(&record_key(id), &record);
    if let Some(settings) = &settings {
        storage::write(KEY_CONFIG, settings);
    }
    sdk::emit(&("execute", id, executor.account));
    match action {
        Action::Transfer(asset, to, amount) => {
            sdk::asset_transfer(&asset, &to, amount);
            Ok(Vec::new())
        }
        // A failing callee fails the whole operation, and the proposal stays pending
        Action::Call(contract, method, args, funds) => {
            Ok(sdk::call_with_funds(&contract, &method, &args, &funds))
        }
        Action::AddOwner(account) => {
            sdk::emit(&("owner_added", account));
            Ok(Vec::new())
        }
        Action::RemoveOwner(account) => {
            sdk::emit(&("owner_removed", account));
            Ok(Vec::new())
        }
        Action::SetThreshold(threshold) => {
            sdk::emit(&("threshold", threshold));
            Ok(Vec::new())
        }
    }
}

/// `cancel(id)`: the proposer withdraws a pending proposal.
pub fn cancel(id: u64) -> Result<(), Vec<u8>> {
    no_funds()?;
    let (config, caller) = as_owner()?;
    let mut record = pending(id)?;
    if record.proposer != caller.account {
        return Err(err(ERR_NOT_PROPOSER));
    }
    record.approvals = config.counted(&record.approvals);
    record.status = Status::Cancelled;
    storage::write(&record_key(id), &record);
    sdk::emit(&("cancel", id));
    Ok(())
}

/// `wallet() -> Wallet`.
pub fn wallet() -> Result<Wallet, Vec<u8>> {
    no_funds()?;
    let config = load_config()?;
    Ok(Wallet {
        owners: config.owners.iter().map(|m| m.account).collect(),
        threshold: config.threshold,
        proposals: config.proposals,
    })
}

/// The proposal as the views show it.
fn view(config: &Config, id: u64, record: Record) -> Result<Proposal, Vec<u8>> {
    let approvals = if record.status == Status::Pending {
        config.counted(&record.approvals)
    } else {
        record.approvals
    };
    Ok(Proposal {
        id,
        proposer: record.proposer,
        action: load_action(id)?,
        approvals: approvals.iter().map(|a| a.account).collect(),
        status: record.status,
        created_at: record.created_at,
    })
}

/// `proposal(id) -> Option<Proposal>`.
pub fn proposal(id: u64) -> Result<Option<Proposal>, Vec<u8>> {
    no_funds()?;
    let config = load_config()?;
    match storage::read::<Record>(&record_key(id))? {
        Some(record) => Ok(Some(view(&config, id, record)?)),
        None => Ok(None),
    }
}

/// `proposals(start, limit) -> Vec<Proposal>`: by ID from `start` (0 means 1), at most `limit` and
/// [`MAX_PAGE_ITEMS`], and within [`MAX_PAGE_BYTES`].
pub fn proposals(start: u64, limit: u32) -> Result<Vec<Proposal>, Vec<u8>> {
    no_funds()?;
    let config = load_config()?;
    let count = usize::try_from(limit.min(MAX_PAGE_ITEMS)).map_err(|_| overflow())?;
    let mut page = Vec::new();
    let mut bytes = 0usize;
    for id in (start.max(1)..=config.proposals).take(count) {
        let proposal = view(&config, id, load_record(id)?)?;
        bytes = bytes
            .checked_add(sdk::encode(&proposal).len())
            .ok_or_else(overflow)?;
        if bytes > MAX_PAGE_BYTES {
            break;
        }
        page.push(proposal);
    }
    Ok(page)
}

/// `balance(asset) -> u128`: the wallet's balance of `asset`.
pub fn balance(asset: &[u8; 32]) -> Result<u128, Vec<u8>> {
    no_funds()?;
    load_config()?;
    Ok(sdk::asset_balance(asset, &sdk::self_id()))
}
