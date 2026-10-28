//! Tutorial contract: a poll. The account that deploys it asks a question with 2 to 8 options; every account votes
//! once while the poll is open, and the creator closes it. It takes no funds.
//!
//! The contract of the docs' tutorial "Your first contract", which builds it in the playground in place of the
//! counter's source, so it needs nothing but the contract SDK. It keeps its state under a few storage keys and one key
//! per voter, refuses with a reason what it does not allow, knows its creator and its voters by `caller()`, and emits
//! an event for every change.
//!
//! Arguments and return values are Borsh-encoded; the options are numbered from 0 in the order `init` takes them.
//!
//! ```abi
//! init(question: String, options: Vec<String>)   // a question of 1 to 200 bytes; 2 to 8 options of 1 to 64 bytes
//! call vote(option: u32) -> u64                   // one vote per account while the poll is open; returns the option's votes
//! call close()                                    // the creator only: ends the voting for good
//! view poll() -> (String, Vec<String>, bool)      // the question, the options, whether voting is open
//! view results() -> Vec<u64>                      // the votes for each option, in order
//! view voted(account: Account) -> Option<u32>     // the option the account voted for; None before it votes
//! event voted(voter: Account, option: u32)
//! event closed(total: u64)
//! ```
#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::{self as sdk, storage, CallResult, Decode};

sdk::entry!(dispatch);

/// The longest question, in bytes.
pub const MAX_QUESTION: usize = 200;
/// The longest option, in bytes.
pub const MAX_OPTION: usize = 64;
/// The most options a poll has; it has at least 2.
pub const MAX_OPTIONS: usize = 8;

const KEY_QUESTION: &[u8] = b"question";
const KEY_OPTIONS: &[u8] = b"options";
const KEY_CREATOR: &[u8] = b"creator";
const KEY_OPEN: &[u8] = b"open";
const KEY_TALLY: &[u8] = b"tally";
/// Followed by a voter's account: the option it voted for.
const PREFIX_VOTE: &[u8] = b"vote:";

/// Dispatches the poll's methods; storage belongs to this contract instance.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    if !sdk::call_funds().is_empty() {
        return sdk::revert("unexpected attached funds");
    }
    match method {
        "init" => {
            let (question, options): (String, Vec<String>) = sdk::decode(args)?;
            init(question, options)
        }
        "vote" => vote(sdk::decode(args)?),
        "close" => {
            sdk::decode::<()>(args)?;
            close()
        }
        "poll" => {
            sdk::decode::<()>(args)?;
            let question: String = load(KEY_QUESTION)?;
            let options: Vec<String> = load(KEY_OPTIONS)?;
            let open: bool = load(KEY_OPEN)?;
            sdk::ok(&(question, options, open))
        }
        "results" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&load::<Vec<u64>>(KEY_TALLY)?)
        }
        "voted" => {
            let account: [u8; 32] = sdk::decode(args)?;
            sdk::ok(&storage::read::<u32>(&vote_key(&account))?)
        }
        _ => sdk::revert("unknown method"),
    }
}

/// Keeps the question and the options and opens the poll with no votes; the deploying account is its creator.
fn init(question: String, options: Vec<String>) -> CallResult {
    if question.is_empty() || question.len() > MAX_QUESTION {
        return sdk::revert("the question must be 1 to 200 bytes");
    }
    if options.len() < 2 || options.len() > MAX_OPTIONS {
        return sdk::revert("a poll has 2 to 8 options");
    }
    if options.iter().any(|o| o.is_empty() || o.len() > MAX_OPTION) {
        return sdk::revert("an option must be 1 to 64 bytes");
    }
    storage::write(KEY_TALLY, &alloc::vec![0u64; options.len()]);
    storage::write(KEY_QUESTION, &question);
    storage::write(KEY_OPTIONS, &options);
    storage::write(KEY_CREATOR, &sdk::caller());
    storage::write(KEY_OPEN, &true);
    sdk::ok(&())
}

/// Counts the caller's vote for `option`, once per account while the poll is open.
fn vote(option: u32) -> CallResult {
    if !load::<bool>(KEY_OPEN)? {
        return sdk::revert("voting is closed");
    }
    let voter = sdk::caller();
    let key = vote_key(&voter);
    if storage::get(&key).is_some() {
        return sdk::revert("already voted");
    }
    let mut tally: Vec<u64> = load(KEY_TALLY)?;
    let Some(count) = usize::try_from(option).ok().and_then(|i| tally.get_mut(i)) else {
        return sdk::revert("unknown option");
    };
    let Some(votes) = count.checked_add(1) else {
        return sdk::revert("too many votes");
    };
    *count = votes;
    storage::write(KEY_TALLY, &tally);
    storage::write(&key, &option);
    sdk::emit(&("voted", voter, option));
    sdk::ok(&votes)
}

/// Ends the voting; only the creator may, once.
fn close() -> CallResult {
    if load::<[u8; 32]>(KEY_CREATOR)? != sdk::caller() {
        return sdk::revert("only the creator");
    }
    if !load::<bool>(KEY_OPEN)? {
        return sdk::revert("voting is closed");
    }
    let tally: Vec<u64> = load(KEY_TALLY)?;
    let Some(total) = tally.iter().try_fold(0u64, |sum, n| sum.checked_add(*n)) else {
        return sdk::revert("too many votes");
    };
    storage::write(KEY_OPEN, &false);
    sdk::emit(&("closed", total));
    sdk::ok(&())
}

/// Reads a value `init` stored.
fn load<T: Decode>(key: &[u8]) -> Result<T, Vec<u8>> {
    storage::read(key)?.ok_or_else(|| b"not initialized".to_vec())
}

/// The storage key of `account`'s vote.
fn vote_key(account: &[u8; 32]) -> Vec<u8> {
    [PREFIX_VOTE, account.as_slice()].concat()
}
