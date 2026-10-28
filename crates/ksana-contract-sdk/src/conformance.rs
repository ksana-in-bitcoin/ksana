//! Checks a contract against KS-20 or KS-721 on the mock host (`docs/STANDARDS.md`, D-120).
//!
//! [`ks20`] and [`ks721`] run the rules every implementation of the standard keeps, whatever its own rules are, each in
//! a fresh mock host: the declaration and the metadata, events that account for every change, the refusals and their
//! reasons, allowances and operators and their expiry, attached funds, `transfer_and_call` with a receiving contract
//! the kit registers, and the extensions the contract declares. A contract's native tests call them with a setup that
//! deploys it:
//!
//! ```ignore
//! use ksana_contract_sdk::{conformance, encode, mock};
//!
//! #[test]
//! fn the_token_follows_ks20() {
//!     let holder = [7u8; 32];
//!     conformance::ks20(|| {
//!         let init = encode(&("Token", "TKN", 8u8, 1_000_000u128));
//!         let token = mock::deploy(holder, 0, b"token", my_token::dispatch, &init).expect("deploys");
//!         conformance::Ks20Subject { token, holder }
//!     })
//!     .unwrap();
//! }
//! ```
//!
//! The setup runs once for every check, in a mock host [`mock::reset`] has just cleared. It deploys the contract and
//! returns it with an account that holds at least [`MIN_BALANCE`] base units (KS-20) or at least [`MIN_TOKENS`]
//! tokens (KS-721). The checks move tokens between that account and the kit's own accounts ([`ALICE`], [`BOB`],
//! [`RECEIVER`]), which hold nothing at first: a contract whose own rules would refuse those moves (an allowlist, a
//! pause) lets them through in the setup, and a mintable token leaves room for 5 more base units under its cap. A
//! failure names the check and what differed; the checks stop at the first.

use std::collections::{BTreeMap, BTreeSet};

use crate::codec::{decode, encode, Decode};
use crate::ks721::TokenId;
use crate::mock::{self, Failure};
use crate::standard::{
    reply, CAPABILITIES, CAP_FEE, CAP_MINT, ERR_MINTING_ENDED, ERR_NOT_THE_MINTER,
    ERR_SELF_RECIPIENT, ERR_UNEXPECTED_FUNDS, ERR_UNKNOWN_METHOD, ERR_ZERO_ACCOUNT, MAX_NAME_BYTES,
    MAX_SYMBOL_BYTES, ZERO_ACCOUNT,
};
use crate::{ks20, ks721, Funds};

/// The least the KS-20 setup's holder holds, in base units.
pub const MIN_BALANCE: u128 = 100;
/// The fewest tokens the KS-721 setup's holder owns.
pub const MIN_TOKENS: usize = 2;
/// An account of the kit: it receives, spends allowances and operates.
pub const ALICE: [u8; 32] = [0xA1; 32];
/// Another account of the kit.
pub const BOB: [u8; 32] = [0xB2; 32];
/// The contract the kit registers to receive `transfer_and_call`.
pub const RECEIVER: [u8; 32] = [0xC3; 32];
/// The built-in asset the kit attaches to calls.
pub const FUNDS_ASSET: [u8; 32] = [0xF4; 32];
/// A token ID no setup mints.
pub const UNKNOWN_TOKEN: TokenId = [0xE5; 32];

/// What a check found wrong: the check's name and what differed.
pub type Outcome = Result<(), String>;

/// A KS-20 token deployed by a setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ks20Subject {
    /// The token contract.
    pub token: [u8; 32],
    /// An account holding at least [`MIN_BALANCE`] base units, that the token's rules let send to the kit's accounts.
    pub holder: [u8; 32],
}

/// A KS-721 collection deployed by a setup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ks721Subject {
    /// The collection contract.
    pub collection: [u8; 32],
    /// An account owning at least [`MIN_TOKENS`] tokens, that the collection's rules let send to the kit's accounts.
    pub holder: [u8; 32],
    /// Tokens `holder` owns, at least [`MIN_TOKENS`].
    pub tokens: Vec<TokenId>,
}

fn fail<T>(what: impl Into<String>) -> Result<T, String> {
    Err(what.into())
}

fn ensure(condition: bool, what: impl FnOnce() -> String) -> Outcome {
    if condition {
        Ok(())
    } else {
        Err(what())
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A named check of a subject.
type Check<S> = (&'static str, fn(&S) -> Outcome);

/// Runs `checks` in order, each in a fresh host with its own setup; the first failure, named.
fn run_checks<S>(setup: &dyn Fn() -> S, checks: &[Check<S>]) -> Outcome {
    for (name, check) in checks {
        mock::reset();
        let subject = setup();
        check(&subject).map_err(|what| format!("{name}: {what}"))?;
    }
    mock::reset();
    Ok(())
}

/// A view of `contract`, called by [`BOB`], decoded as `T`.
fn view<T: Decode>(contract: &[u8; 32], method: &str, args: &[u8]) -> Result<T, String> {
    let out = mock::call(BOB, *contract, method, args)
        .map_err(|failure| format!("{method} failed: {failure:?}"))?;
    decode(&out)
        .map_err(|_| format!("{method} answered bytes that do not decode as its return type"))
}

/// Expects the call `result` to have reverted in `contract` with `reason`.
fn reverted(
    result: Result<Vec<u8>, Failure>,
    contract: &[u8; 32],
    reason: &[u8],
    what: &str,
) -> Outcome {
    match result {
        Err(Failure::Revert {
            contract: by,
            reason: got,
        }) if by == *contract && got == reason => Ok(()),
        other => fail(format!(
            "{what}: expected the revert `{}`, got {other:?}",
            text(reason)
        )),
    }
}

/// Expects the call `result` to have succeeded.
fn succeeded(result: Result<Vec<u8>, Failure>, what: &str) -> Result<Vec<u8>, String> {
    result.map_err(|failure| format!("{what} failed: {failure:?}"))
}

/// The name an event starts with, if it starts with a Borsh string.
fn event_name(data: &[u8]) -> Option<String> {
    let mut input = data;
    String::decode(&mut input)
}

/// The events of `contract` since the host was last reset or its events cleared, as `parse` reads those named `names`;
/// the contract's other events are its own business.
fn standard_events<E>(
    contract: &[u8; 32],
    names: &[&str],
    parse: fn(&[u8]) -> Option<E>,
) -> Result<Vec<E>, String> {
    let mut out = Vec::new();
    for event in mock::events()
        .into_iter()
        .filter(|e| e.contract == *contract)
    {
        let Some(name) = event_name(&event.data) else {
            continue;
        };
        if !names.contains(&name.as_str()) {
            continue;
        }
        out.push(parse(&event.data).ok_or_else(|| {
            format!("an event named `{name}` does not have the standard's layout")
        })?);
    }
    Ok(out)
}

/// The capabilities view: decodes, names each once, the known names in their order.
fn check_capabilities(contract: &[u8; 32]) -> Result<Vec<String>, String> {
    let names: Vec<String> = view(contract, "capabilities", &[])?;
    let unique: BTreeSet<&String> = names.iter().collect();
    ensure(unique.len() == names.len(), || {
        format!("capabilities() names one capability twice: {names:?}")
    })?;
    let known: Vec<&String> = names
        .iter()
        .filter(|name| CAPABILITIES.contains(&name.as_str()))
        .collect();
    let in_order = known.windows(2).all(|pair| match pair {
        [a, b] => {
            let rank = |name: &String| CAPABILITIES.iter().position(|c| *c == name.as_str());
            rank(a) < rank(b)
        }
        _ => true,
    });
    ensure(in_order, || {
        format!("capabilities() does not list the known names in their order: {names:?}")
    })?;
    Ok(names)
}

fn check_name_and_symbol(name: &str, symbol: &str) -> Outcome {
    ensure(!name.is_empty() && name.len() <= MAX_NAME_BYTES, || {
        format!("the name `{name}` is not 1 to {MAX_NAME_BYTES} bytes")
    })?;
    ensure(
        !symbol.is_empty() && symbol.len() <= MAX_SYMBOL_BYTES,
        || format!("the symbol `{symbol}` is not 1 to {MAX_SYMBOL_BYTES} bytes"),
    )
}

/// Registers [`FUNDS_ASSET`] and gives `holder` 10 of it.
fn fund(holder: &[u8; 32]) -> Outcome {
    mock::register_asset(FUNDS_ASSET, "Conformance funds", 0, ZERO_ACCOUNT);
    mock::mint(FUNDS_ASSET, *holder, 10).map_err(|failure| format!("funding failed: {failure:?}"))
}

fn funds(amount: u128) -> [Funds; 1] {
    [Funds {
        asset: FUNDS_ASSET,
        amount,
    }]
}

/// What the kit's receiver saw of its last call.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    caller: [u8; 32],
    args: Vec<u8>,
    funds: Vec<Funds>,
    /// The contract's balance (KS-20) or the token's owner (KS-721) in the middle of the call.
    probe: Vec<u8>,
}

const SEEN: &[u8] = b"seen";

/// Takes the sender's data out of the arguments of `on_token_received` or `on_nft_received`.
type DataOf = fn(&[u8]) -> Result<Vec<u8>, Vec<u8>>;
/// Reads the sender contract (its caller) in the middle of the receiver's call, given the call's arguments.
type Probe = fn(&[u8; 32], &[u8]) -> Vec<u8>;

/// Registers the receiver at [`RECEIVER`]: its method `method` records what it sees and answers
/// `reply(&("answer", data))`, or refuses with `refused` when the data is `refuse`. `data_of` takes the data out of the
/// arguments, and `probe` reads the sender contract in the middle of the call.
fn register_receiver(method: &'static str, data_of: DataOf, probe: Probe) {
    mock::register(RECEIVER, move |name, args| {
        if name != method {
            return Err(ERR_UNKNOWN_METHOD.to_vec());
        }
        let caller = crate::caller();
        let seen = (
            caller,
            args.to_vec(),
            crate::call_funds(),
            probe(&caller, args),
        );
        crate::storage::write(SEEN, &seen);
        let data = data_of(args)?;
        if data == b"refuse" {
            return Err(b"refused".to_vec());
        }
        reply(&("answer", data))
    });
}

fn seen() -> Result<Seen, String> {
    let raw = mock::storage_value(&RECEIVER, SEEN).ok_or("the receiver was not called")?;
    let (caller, args, funds, probe): ([u8; 32], Vec<u8>, Vec<Funds>, Vec<u8>) =
        decode(&raw).map_err(|_| "the receiver's record does not decode".to_string())?;
    Ok(Seen {
        caller,
        args,
        funds,
        probe,
    })
}

/// Checks that `out`, the answer of `transfer_and_call` with `data`, is the receiver's answer unchanged.
fn check_answer(out: &[u8], data: &[u8]) -> Outcome {
    let expected = encode(&encode(&("answer", data)));
    ensure(out == expected.as_slice(), || {
        "transfer_and_call did not answer the receiver's Bytes unchanged".to_string()
    })
}

/// Checks the funds the receiver saw and where they are now.
fn check_forwarded(seen: &Seen, sender: &[u8; 32], holder: &[u8; 32]) -> Outcome {
    ensure(seen.funds == funds(4), || {
        format!(
            "the receiver was given {:?}, not the 4 attached funds",
            seen.funds
        )
    })?;
    ensure(
        mock::balance(&FUNDS_ASSET, &RECEIVER) == 4
            && mock::balance(&FUNDS_ASSET, sender) == 0
            && mock::balance(&FUNDS_ASSET, holder) == 6,
        || "the attached funds did not end with the receiver".to_string(),
    )
}

// ---------------------------------------------------------------------------
// KS-20

const KS20_EVENTS: [&str; 5] = ["standards", "metadata", "transfer", "approve", "minter"];

fn ks20_events(token: &[u8; 32]) -> Result<Vec<ks20::Event>, String> {
    standard_events(token, &KS20_EVENTS, ks20::Event::parse)
}

/// The balance changes `events` record: each account's in and out, and the supply created and destroyed.
#[derive(Debug, Default)]
struct Flows {
    accounts: BTreeMap<[u8; 32], (u128, u128)>,
    minted: u128,
    burned: u128,
}

impl Flows {
    fn of(events: &[ks20::Event]) -> Result<Self, String> {
        let overflow = || "the transfer events add up past u128".to_string();
        let mut flows = Self::default();
        for event in events {
            let ks20::Event::Transfer { from, to, amount } = event else {
                continue;
            };
            if *amount == 0 {
                return fail("a transfer event of 0");
            }
            if *from == ZERO_ACCOUNT {
                flows.minted = flows.minted.checked_add(*amount).ok_or_else(overflow)?;
            } else {
                let entry = flows.accounts.entry(*from).or_default();
                entry.1 = entry.1.checked_add(*amount).ok_or_else(overflow)?;
            }
            if *to == ZERO_ACCOUNT {
                flows.burned = flows.burned.checked_add(*amount).ok_or_else(overflow)?;
            } else {
                let entry = flows.accounts.entry(*to).or_default();
                entry.0 = entry.0.checked_add(*amount).ok_or_else(overflow)?;
            }
        }
        Ok(flows)
    }

    /// What `account` received net (in − out); `None` when it lost.
    fn gain(&self, account: &[u8; 32]) -> Option<u128> {
        let (received, sent) = self.accounts.get(account).copied().unwrap_or((0, 0));
        received.checked_sub(sent)
    }

    fn loss(&self, account: &[u8; 32]) -> Option<u128> {
        let (received, sent) = self.accounts.get(account).copied().unwrap_or((0, 0));
        sent.checked_sub(received)
    }
}

fn balance_of(token: &[u8; 32], account: &[u8; 32]) -> Result<u128, String> {
    view(token, "balance_of", &encode(account))
}

fn total_supply(token: &[u8; 32]) -> Result<u128, String> {
    view(token, "total_supply", &[])
}

/// Runs `op`, then checks that the token's events account for every change of balance of `watched` and of the supply.
fn accounted(
    token: &[u8; 32],
    watched: &[[u8; 32]],
    op: impl FnOnce() -> Result<Vec<u8>, Failure>,
    what: &str,
) -> Result<(Vec<u8>, Vec<ks20::Event>, Flows), String> {
    let before: Vec<u128> = watched
        .iter()
        .map(|account| balance_of(token, account))
        .collect::<Result<_, _>>()?;
    let supply_before = total_supply(token)?;
    mock::clear_events();
    let out = succeeded(op(), what)?;
    let events = ks20_events(token)?;
    let flows = Flows::of(&events)?;
    for (account, before) in watched.iter().zip(before) {
        let (received, sent) = flows.accounts.get(account).copied().unwrap_or((0, 0));
        let expected = before
            .checked_add(received)
            .and_then(|v| v.checked_sub(sent));
        let after = balance_of(token, account)?;
        ensure(expected == Some(after), || {
            format!("{what}: a balance went from {before} to {after}, the events say {expected:?}")
        })?;
    }
    let supply = supply_before
        .checked_add(flows.minted)
        .and_then(|v| v.checked_sub(flows.burned));
    let after = total_supply(token)?;
    ensure(supply == Some(after), || {
        format!(
            "{what}: the supply went from {supply_before} to {after}, the events say {supply:?}"
        )
    })?;
    Ok((out, events, flows))
}

fn has_fee(token: &[u8; 32]) -> Result<bool, String> {
    Ok(check_capabilities(token)?
        .iter()
        .any(|name| name == CAP_FEE))
}

/// Checks a transfer of `amount` from `from` to `to` the events show: `from` lost exactly `amount`, `to` received the
/// rest after the fee (all of it without the `fee` capability, in one event); returns what `to` received.
fn check_sent(
    token: &[u8; 32],
    flows: &Flows,
    events: &[ks20::Event],
    from: &[u8; 32],
    to: &[u8; 32],
    amount: u128,
    what: &str,
) -> Result<u128, String> {
    ensure(flows.loss(from) == Some(amount), || {
        format!("{what}: the sender did not lose exactly {amount}")
    })?;
    let net = flows.gain(to).unwrap_or(0);
    ensure(net > 0 && net <= amount, || {
        format!("{what}: the recipient received {net} of {amount}")
    })?;
    if !has_fee(token)? {
        ensure(net == amount, || {
            format!("{what}: a token without the `fee` capability gave {net} of {amount}")
        })?;
        let expected = [ks20::Event::Transfer {
            from: *from,
            to: *to,
            amount,
        }];
        ensure(events == expected.as_slice(), || {
            format!("{what}: the events are {events:?}, not one transfer")
        })?;
    }
    Ok(net)
}

fn transfer(
    token: &[u8; 32],
    caller: &[u8; 32],
    to: &[u8; 32],
    amount: u128,
) -> Result<Vec<u8>, Failure> {
    mock::call(*caller, *token, "transfer", &encode(&(*to, amount)))
}

fn transfer_from(
    token: &[u8; 32],
    caller: &[u8; 32],
    from: &[u8; 32],
    to: &[u8; 32],
    amount: u128,
) -> Result<Vec<u8>, Failure> {
    mock::call(
        *caller,
        *token,
        "transfer_from",
        &encode(&(*from, *to, amount)),
    )
}

fn approve(
    token: &[u8; 32],
    owner: &[u8; 32],
    spender: &[u8; 32],
    amount: u128,
    expires_at: Option<u64>,
) -> Result<Vec<u8>, Failure> {
    mock::call(
        *owner,
        *token,
        "approve",
        &encode(&(*spender, amount, expires_at)),
    )
}

fn allowance(token: &[u8; 32], owner: &[u8; 32], spender: &[u8; 32]) -> Result<u128, String> {
    view(token, "allowance", &encode(&(*owner, *spender)))
}

fn allowance_info(
    token: &[u8; 32],
    owner: &[u8; 32],
    spender: &[u8; 32],
) -> Result<(u128, Option<u64>), String> {
    view(token, "allowance_info", &encode(&(*owner, *spender)))
}

fn standards_of(contract: &[u8; 32]) -> Result<Vec<String>, String> {
    view(contract, "standards", &[])
}

fn ks20_declaration(s: &Ks20Subject) -> Outcome {
    let events = ks20_events(&s.token)?;
    let mut events = events.iter();
    let Some(ks20::Event::Standards(ids)) = events.next() else {
        return fail("the first standard event of the deployment is not `standards`");
    };
    ensure(ids.first().map(String::as_str) == Some(ks20::ID), || {
        format!("the declaration {ids:?} does not start with `KS-20`")
    })?;
    ensure(!ids.iter().any(|id| id == ks721::ID), || {
        "the declaration names KS-721 too".to_string()
    })?;
    let Some(ks20::Event::Metadata {
        name,
        symbol,
        decimals,
    }) = events.next()
    else {
        return fail("the second standard event of the deployment is not `metadata`");
    };
    ensure(
        !events.any(|e| matches!(e, ks20::Event::Standards(_))),
        || "the deployment declares twice".to_string(),
    )?;
    ensure(standards_of(&s.token)? == *ids, || {
        "standards() differs from the declaration".to_string()
    })?;
    let metadata: (String, String, u8) = view(&s.token, "metadata", &[])?;
    ensure(
        metadata == (name.clone(), symbol.clone(), *decimals),
        || format!("metadata() is {metadata:?}, the event says ({name}, {symbol}, {decimals})"),
    )?;
    check_name_and_symbol(name, symbol)?;
    ensure(*decimals <= ks20::MAX_DECIMALS, || {
        format!("{decimals} decimals")
    })?;
    check_capabilities(&s.token)?;
    Ok(())
}

fn ks20_supply(s: &Ks20Subject) -> Outcome {
    let flows = Flows::of(&ks20_events(&s.token)?)?;
    let mut sum: u128 = 0;
    for account in flows.accounts.keys() {
        let expected = flows
            .gain(account)
            .ok_or("the deployment's events take more from an account than they give it")?;
        let balance = balance_of(&s.token, account)?;
        ensure(balance == expected, || {
            format!("an account holds {balance}, the deployment's events say {expected}")
        })?;
        sum = sum
            .checked_add(balance)
            .ok_or("the balances add up past u128")?;
    }
    let supply = total_supply(&s.token)?;
    ensure(supply == sum, || {
        format!("total_supply() is {supply}, the balances add up to {sum}")
    })?;
    ensure(
        flows.minted.checked_sub(flows.burned) == Some(supply),
        || "the supply differs from what the events minted and burned".to_string(),
    )?;
    let held = balance_of(&s.token, &s.holder)?;
    ensure(held >= MIN_BALANCE, || {
        format!("the holder holds {held}, less than {MIN_BALANCE}")
    })?;
    for account in [ALICE, BOB, RECEIVER] {
        ensure(balance_of(&s.token, &account)? == 0, || {
            "a kit account holds tokens after the setup".to_string()
        })?;
    }
    Ok(())
}

fn ks20_unknown_method(s: &Ks20Subject) -> Outcome {
    let result = mock::call(BOB, s.token, "ks20_conformance_no_such_method", &[]);
    reverted(result, &s.token, ERR_UNKNOWN_METHOD, "an unknown method")
}

fn ks20_transfer(s: &Ks20Subject) -> Outcome {
    let (out, events, flows) = accounted(
        &s.token,
        &[s.holder, ALICE],
        || transfer(&s.token, &s.holder, &ALICE, 10),
        "transfer",
    )?;
    ensure(out.is_empty(), || "transfer answered something".to_string())?;
    check_sent(&s.token, &flows, &events, &s.holder, &ALICE, 10, "transfer")?;
    Ok(())
}

fn ks20_refusals(s: &Ks20Subject) -> Outcome {
    let t = &s.token;
    reverted(
        transfer(t, &s.holder, &ALICE, 0),
        t,
        ks20::ERR_ZERO_AMOUNT,
        "a transfer of 0",
    )?;
    reverted(
        transfer(t, &s.holder, &ZERO_ACCOUNT, 1),
        t,
        ERR_ZERO_ACCOUNT,
        "a transfer to the zero account",
    )?;
    reverted(
        transfer(t, &s.holder, t, 1),
        t,
        ERR_SELF_RECIPIENT,
        "a transfer to the token itself",
    )?;
    reverted(
        transfer(t, &ALICE, &BOB, 1),
        t,
        ks20::ERR_INSUFFICIENT_BALANCE,
        "a transfer beyond the balance",
    )?;
    let held = balance_of(t, &s.holder)?;
    let beyond = held.checked_add(1).ok_or("the holder holds u128::MAX")?;
    reverted(
        transfer(t, &s.holder, &ALICE, beyond),
        t,
        ks20::ERR_INSUFFICIENT_BALANCE,
        "a transfer of the balance and 1",
    )?;
    fund(&s.holder)?;
    let paid = mock::call_with_funds(
        s.holder,
        *t,
        "transfer",
        &encode(&(ALICE, 1u128)),
        &funds(1),
    );
    reverted(
        paid,
        t,
        ERR_UNEXPECTED_FUNDS,
        "a transfer with attached funds",
    )?;
    let paid = mock::call_with_funds(
        s.holder,
        *t,
        "approve",
        &encode(&(ALICE, 1u128, None::<u64>)),
        &funds(1),
    );
    reverted(
        paid,
        t,
        ERR_UNEXPECTED_FUNDS,
        "an approve with attached funds",
    )?;
    let paid = mock::call_with_funds(s.holder, *t, "balance_of", &encode(&ALICE), &funds(1));
    reverted(paid, t, ERR_UNEXPECTED_FUNDS, "a view with attached funds")
}

fn ks20_allowances(s: &Ks20Subject) -> Outcome {
    let t = &s.token;
    mock::clear_events();
    succeeded(approve(t, &s.holder, &ALICE, 5, None), "approve")?;
    let expected = [ks20::Event::Approve {
        owner: s.holder,
        spender: ALICE,
        amount: 5,
        expires_at: None,
    }];
    let events = ks20_events(t)?;
    ensure(events == expected, || format!("approve emitted {events:?}"))?;
    ensure(allowance(t, &s.holder, &ALICE)? == 5, || {
        "the allowance is not 5".to_string()
    })?;
    ensure(allowance_info(t, &s.holder, &ALICE)? == (5, None), || {
        "allowance_info is not (5, None)".to_string()
    })?;
    // set, not added
    succeeded(approve(t, &s.holder, &ALICE, 3, None), "a second approve")?;
    ensure(allowance(t, &s.holder, &ALICE)? == 3, || {
        "a second approve added to the allowance instead of setting it".to_string()
    })?;
    let (_, events, flows) = accounted(
        t,
        &[s.holder, ALICE, BOB],
        || transfer_from(t, &ALICE, &s.holder, &BOB, 2),
        "transfer_from",
    )?;
    check_sent(t, &flows, &events, &s.holder, &BOB, 2, "transfer_from")?;
    ensure(
        !events
            .iter()
            .any(|e| matches!(e, ks20::Event::Approve { .. })),
        || "transfer_from emitted approve".to_string(),
    )?;
    ensure(allowance(t, &s.holder, &ALICE)? == 1, || {
        "transfer_from did not take 2 of the allowance".to_string()
    })?;
    reverted(
        transfer_from(t, &ALICE, &s.holder, &BOB, 2),
        t,
        ks20::ERR_INSUFFICIENT_ALLOWANCE,
        "a transfer_from beyond the allowance",
    )?;
    reverted(
        transfer_from(t, &BOB, &s.holder, &BOB, 1),
        t,
        ks20::ERR_INSUFFICIENT_ALLOWANCE,
        "a transfer_from without an allowance",
    )?;
    accounted(
        t,
        &[s.holder, BOB],
        || transfer_from(t, &s.holder, &s.holder, &BOB, 1),
        "a transfer_from of one's own tokens",
    )?;
    succeeded(approve(t, &s.holder, &ALICE, 0, None), "a revoking approve")?;
    ensure(allowance(t, &s.holder, &ALICE)? == 0, || {
        "approve of 0 did not revoke the allowance".to_string()
    })?;
    ensure(allowance_info(t, &s.holder, &ALICE)?.0 == 0, || {
        "allowance_info still holds an amount after approve of 0".to_string()
    })?;
    reverted(
        approve(t, &s.holder, &ZERO_ACCOUNT, 1, None),
        t,
        ERR_ZERO_ACCOUNT,
        "an approve of the zero account",
    )?;
    // unlimited: never goes down
    succeeded(
        approve(t, &s.holder, &ALICE, ks20::UNLIMITED, None),
        "an unlimited approve",
    )?;
    succeeded(
        transfer_from(t, &ALICE, &s.holder, &BOB, 3),
        "a transfer_from of an unlimited allowance",
    )?;
    ensure(allowance(t, &s.holder, &ALICE)? == ks20::UNLIMITED, || {
        "transfer_from took from an unlimited allowance".to_string()
    })
}

fn ks20_expiry(s: &Ks20Subject) -> Outcome {
    let t = &s.token;
    mock::set_block_timestamp(1_000);
    succeeded(
        approve(t, &s.holder, &ALICE, 5, Some(1_500)),
        "an approve with an expiry",
    )?;
    ensure(allowance(t, &s.holder, &ALICE)? == 5, || {
        "the allowance is not 5".to_string()
    })?;
    ensure(
        allowance_info(t, &s.holder, &ALICE)? == (5, Some(1_500)),
        || "allowance_info is not (5, Some(1500))".to_string(),
    )?;
    mock::set_block_timestamp(1_499);
    succeeded(
        transfer_from(t, &ALICE, &s.holder, &BOB, 1),
        "a transfer_from before the expiry",
    )?;
    mock::set_block_timestamp(1_500);
    ensure(allowance(t, &s.holder, &ALICE)? == 0, || {
        "allowance() is not 0 at the expiry".to_string()
    })?;
    ensure(
        allowance_info(t, &s.holder, &ALICE)? == (4, Some(1_500)),
        || "allowance_info is not (4, Some(1500)) at the expiry".to_string(),
    )?;
    reverted(
        transfer_from(t, &ALICE, &s.holder, &BOB, 1),
        t,
        ks20::ERR_ALLOWANCE_EXPIRED,
        "a transfer_from at the expiry",
    )
}

fn ks20_data(args: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    ks20::received(args).map(|received| received.data)
}

fn ks20_probe(token: &[u8; 32], _args: &[u8]) -> Vec<u8> {
    encode(&ks20::Token(*token).balance_of(&crate::self_id()).ok())
}

fn ks20_transfer_and_call(s: &Ks20Subject) -> Outcome {
    let t = &s.token;
    fund(&s.holder)?;
    register_receiver(ks20::ON_RECEIVED, ks20_data, ks20_probe);
    let call = |data: &[u8], amount: u128| {
        mock::call_with_funds(
            s.holder,
            *t,
            "transfer_and_call",
            &encode(&(RECEIVER, amount, data)),
            &funds(4),
        )
    };
    reverted(
        call(b"refuse", 7),
        &RECEIVER,
        b"refused",
        "a refused transfer_and_call",
    )?;
    ensure(mock::balance(&FUNDS_ASSET, &s.holder) == 10, || {
        "a refused transfer_and_call kept the attached funds".to_string()
    })?;
    ensure(balance_of(t, &RECEIVER)? == 0, || {
        "a refused transfer_and_call moved tokens".to_string()
    })?;
    let (out, events, flows) = accounted(
        t,
        &[s.holder, RECEIVER],
        || call(b"hello", 7),
        "transfer_and_call",
    )?;
    let net = check_sent(
        t,
        &flows,
        &events,
        &s.holder,
        &RECEIVER,
        7,
        "transfer_and_call",
    )?;
    check_answer(&out, b"hello")?;
    let seen = seen()?;
    ensure(seen.caller == *t, || {
        "the receiver was not called by the token".to_string()
    })?;
    ensure(
        seen.args == encode(&(s.holder, net, b"hello".as_slice())),
        || "the receiver was not given (from, the amount it received, data)".to_string(),
    )?;
    ensure(seen.probe == encode(&Some(net)), || {
        "the receiver's balance was not credited before it was called".to_string()
    })?;
    check_forwarded(&seen, t, &s.holder)?;
    let to_account = mock::call(
        s.holder,
        *t,
        "transfer_and_call",
        &encode(&(BOB, 1u128, b"".as_slice())),
    );
    ensure(to_account.is_err(), || {
        "transfer_and_call to an account that is no contract succeeded".to_string()
    })?;
    reverted(
        call(b"", 0),
        t,
        ks20::ERR_ZERO_AMOUNT,
        "a transfer_and_call of 0",
    )
}

fn ks20_burnable(s: &Ks20Subject) -> Outcome {
    let t = &s.token;
    if !standards_of(t)?.iter().any(|id| id == ks20::BURNABLE) {
        return Ok(());
    }
    let (_, events, _) = accounted(
        t,
        &[s.holder],
        || mock::call(s.holder, *t, "burn", &encode(&3u128)),
        "burn",
    )?;
    let expected = [ks20::Event::Transfer {
        from: s.holder,
        to: ZERO_ACCOUNT,
        amount: 3,
    }];
    ensure(events == expected, || format!("burn emitted {events:?}"))?;
    reverted(
        mock::call(s.holder, *t, "burn", &encode(&0u128)),
        t,
        ks20::ERR_ZERO_AMOUNT,
        "a burn of 0",
    )?;
    let burn_from = || mock::call(ALICE, *t, "burn_from", &encode(&(s.holder, 2u128)));
    reverted(
        burn_from(),
        t,
        ks20::ERR_INSUFFICIENT_ALLOWANCE,
        "a burn_from without an allowance",
    )?;
    succeeded(approve(t, &s.holder, &ALICE, 2, None), "approve")?;
    accounted(t, &[s.holder, ALICE], burn_from, "burn_from")?;
    ensure(allowance(t, &s.holder, &ALICE)? == 0, || {
        "burn_from did not take the allowance".to_string()
    })
}

fn ks20_mintable(s: &Ks20Subject) -> Outcome {
    let t = &s.token;
    if !standards_of(t)?.iter().any(|id| id == ks20::MINTABLE) {
        return Ok(());
    }
    let minter: Option<[u8; 32]> = view(t, "minter", &[])?;
    let _: Option<u128> = view(t, "cap", &[])?;
    let reports_mint = check_capabilities(t)?.iter().any(|name| name == CAP_MINT);
    ensure(reports_mint == minter.is_some(), || {
        "capabilities() reports `mint` exactly while there is a minter".to_string()
    })?;
    let mint = |caller: [u8; 32]| mock::call(caller, *t, "mint", &encode(&(ALICE, 5u128)));
    let Some(minter) = minter else {
        return reverted(
            mint(BOB),
            t,
            ERR_MINTING_ENDED,
            "a mint after minting ended",
        );
    };
    if minter != BOB {
        reverted(
            mint(BOB),
            t,
            ERR_NOT_THE_MINTER,
            "a mint by another account",
        )?;
    }
    let (_, events, _) = accounted(t, &[ALICE], || mint(minter), "mint")?;
    let expected = [ks20::Event::Transfer {
        from: ZERO_ACCOUNT,
        to: ALICE,
        amount: 5,
    }];
    ensure(events == expected, || format!("mint emitted {events:?}"))
}

/// Checks the KS-20 token `setup` deploys (see the module docs).
pub fn ks20(setup: impl Fn() -> Ks20Subject) -> Outcome {
    let checks: [Check<Ks20Subject>; 10] = [
        ("declaration", ks20_declaration),
        ("supply", ks20_supply),
        ("unknown method", ks20_unknown_method),
        ("transfer", ks20_transfer),
        ("refusals", ks20_refusals),
        ("allowances", ks20_allowances),
        ("expiry", ks20_expiry),
        ("transfer_and_call", ks20_transfer_and_call),
        ("KS-20-burnable", ks20_burnable),
        ("KS-20-mintable", ks20_mintable),
    ];
    run_checks(&setup, &checks)
}

// ---------------------------------------------------------------------------
// KS-721

const KS721_EVENTS: [&str; 6] = [
    "standards",
    "metadata",
    "transfer",
    "approve",
    "operator",
    "uri",
];

fn ks721_events(collection: &[u8; 32]) -> Result<Vec<ks721::Event>, String> {
    standard_events(collection, &KS721_EVENTS, ks721::Event::parse)
}

fn owner_of(collection: &[u8; 32], token_id: &TokenId) -> Result<Option<[u8; 32]>, String> {
    view(collection, "owner_of", &encode(token_id))
}

fn nft_balance(collection: &[u8; 32], owner: &[u8; 32]) -> Result<u64, String> {
    view(collection, "balance_of", &encode(owner))
}

fn approved(collection: &[u8; 32], token_id: &TokenId) -> Result<Option<[u8; 32]>, String> {
    view(collection, "approved", &encode(token_id))
}

fn nft_transfer(
    c: &[u8; 32],
    caller: &[u8; 32],
    to: &[u8; 32],
    token_id: &TokenId,
) -> Result<Vec<u8>, Failure> {
    mock::call(*caller, *c, "transfer", &encode(&(*to, *token_id)))
}

fn nft_transfer_from(
    c: &[u8; 32],
    caller: &[u8; 32],
    from: &[u8; 32],
    to: &[u8; 32],
    token_id: &TokenId,
) -> Result<Vec<u8>, Failure> {
    mock::call(
        *caller,
        *c,
        "transfer_from",
        &encode(&(*from, *to, *token_id)),
    )
}

fn nft_approve(
    c: &[u8; 32],
    caller: &[u8; 32],
    spender: Option<[u8; 32]>,
    token_id: &TokenId,
) -> Result<Vec<u8>, Failure> {
    mock::call(*caller, *c, "approve", &encode(&(spender, *token_id)))
}

fn set_operator(
    c: &[u8; 32],
    owner: &[u8; 32],
    operator: &[u8; 32],
    approved: bool,
    expires_at: Option<u64>,
) -> Result<Vec<u8>, Failure> {
    mock::call(
        *owner,
        *c,
        "set_operator",
        &encode(&(*operator, approved, expires_at)),
    )
}

/// The owners the events give, token by token (a burned token has none).
fn owners(events: &[ks721::Event]) -> BTreeMap<TokenId, Option<[u8; 32]>> {
    let mut out = BTreeMap::new();
    for event in events {
        if let ks721::Event::Transfer { to, token_id, .. } = event {
            out.insert(*token_id, (*to != ZERO_ACCOUNT).then_some(*to));
        }
    }
    out
}

fn first_tokens(s: &Ks721Subject) -> Result<(TokenId, TokenId), String> {
    match s.tokens.as_slice() {
        [a, b, ..] => Ok((*a, *b)),
        _ => fail(format!(
            "the setup gives {} tokens, fewer than {MIN_TOKENS}",
            s.tokens.len()
        )),
    }
}

/// Runs `op`, then checks that the collection's events are exactly `expected` (among the standard's) and that the
/// owners and balances moved as they say.
fn moved(
    c: &[u8; 32],
    op: impl FnOnce() -> Result<Vec<u8>, Failure>,
    expected: &[ks721::Event],
    what: &str,
) -> Result<Vec<u8>, String> {
    let mut accounts = BTreeSet::new();
    for event in expected {
        if let ks721::Event::Transfer { from, to, .. } = event {
            accounts.extend([*from, *to].into_iter().filter(|a| *a != ZERO_ACCOUNT));
        }
    }
    let before: BTreeMap<[u8; 32], u64> = accounts
        .iter()
        .map(|account| Ok((*account, nft_balance(c, account)?)))
        .collect::<Result<_, String>>()?;
    mock::clear_events();
    let out = succeeded(op(), what)?;
    let events = ks721_events(c)?;
    ensure(events == expected, || format!("{what} emitted {events:?}"))?;
    for (token_id, owner) in owners(&events) {
        ensure(owner_of(c, &token_id)? == owner, || {
            format!("{what}: owner_of differs from the transfer event")
        })?;
        ensure(approved(c, &token_id)?.is_none(), || {
            format!("{what}: the change of owner did not clear the approval")
        })?;
    }
    for (account, before) in before {
        let sent = events
            .iter()
            .filter(|e| matches!(e, ks721::Event::Transfer { from, .. } if *from == account))
            .count();
        let received = events
            .iter()
            .filter(|e| matches!(e, ks721::Event::Transfer { to, .. } if *to == account))
            .count();
        let expected = u64::try_from(received)
            .ok()
            .and_then(|r| before.checked_add(r))
            .and_then(|v| u64::try_from(sent).ok().and_then(|s| v.checked_sub(s)));
        let after = nft_balance(c, &account)?;
        ensure(expected == Some(after), || {
            format!("{what}: a balance went from {before} to {after}, the events say {expected:?}")
        })?;
    }
    Ok(out)
}

fn ks721_declaration(s: &Ks721Subject) -> Outcome {
    let events = ks721_events(&s.collection)?;
    let mut events = events.iter();
    let Some(ks721::Event::Standards(ids)) = events.next() else {
        return fail("the first standard event of the deployment is not `standards`");
    };
    ensure(ids.first().map(String::as_str) == Some(ks721::ID), || {
        format!("the declaration {ids:?} does not start with `KS-721`")
    })?;
    ensure(!ids.iter().any(|id| id == ks20::ID), || {
        "the declaration names KS-20 too".to_string()
    })?;
    let Some(ks721::Event::Metadata { name, symbol }) = events.next() else {
        return fail("the second standard event of the deployment is not `metadata`");
    };
    ensure(
        !events.any(|e| matches!(e, ks721::Event::Standards(_))),
        || "the deployment declares twice".to_string(),
    )?;
    ensure(standards_of(&s.collection)? == *ids, || {
        "standards() differs from the declaration".to_string()
    })?;
    let metadata: (String, String) = view(&s.collection, "metadata", &[])?;
    ensure(metadata == (name.clone(), symbol.clone()), || {
        format!("metadata() is {metadata:?}, the event says ({name}, {symbol})")
    })?;
    check_name_and_symbol(name, symbol)?;
    check_capabilities(&s.collection)?;
    Ok(())
}

fn ks721_ownership(s: &Ks721Subject) -> Outcome {
    let c = &s.collection;
    first_tokens(s)?;
    let owners = owners(&ks721_events(c)?);
    let mut counts: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    for (token_id, owner) in &owners {
        ensure(owner_of(c, token_id)? == *owner, || {
            "owner_of differs from the deployment's transfer events".to_string()
        })?;
        if let Some(owner) = owner {
            let count = counts.entry(*owner).or_default();
            *count = count.checked_add(1).ok_or("too many tokens")?;
        }
    }
    for (owner, count) in counts {
        let balance = nft_balance(c, &owner)?;
        ensure(balance == count, || {
            format!("an account holds {balance} tokens, the deployment's events say {count}")
        })?;
    }
    for token_id in &s.tokens {
        ensure(owner_of(c, token_id)? == Some(s.holder), || {
            "the holder does not own a token the setup gives".to_string()
        })?;
    }
    ensure(owner_of(c, &UNKNOWN_TOKEN)?.is_none(), || {
        "owner_of answers an owner for a token that does not exist".to_string()
    })?;
    for account in [ALICE, BOB, RECEIVER] {
        ensure(nft_balance(c, &account)? == 0, || {
            "a kit account holds tokens after the setup".to_string()
        })?;
    }
    Ok(())
}

fn ks721_unknown_method(s: &Ks721Subject) -> Outcome {
    let result = mock::call(BOB, s.collection, "ks721_conformance_no_such_method", &[]);
    reverted(
        result,
        &s.collection,
        ERR_UNKNOWN_METHOD,
        "an unknown method",
    )
}

fn ks721_transfer(s: &Ks721Subject) -> Outcome {
    let c = &s.collection;
    let (a, _) = first_tokens(s)?;
    let expected = [ks721::Event::Transfer {
        from: s.holder,
        to: ALICE,
        token_id: a,
    }];
    let out = moved(
        c,
        || nft_transfer(c, &s.holder, &ALICE, &a),
        &expected,
        "transfer",
    )?;
    ensure(out.is_empty(), || "transfer answered something".to_string())
}

fn ks721_refusals(s: &Ks721Subject) -> Outcome {
    let c = &s.collection;
    let (a, _) = first_tokens(s)?;
    reverted(
        nft_transfer(c, &BOB, &ALICE, &a),
        c,
        ks721::ERR_NOT_THE_OWNER,
        "a transfer by another account",
    )?;
    reverted(
        nft_transfer(c, &s.holder, &ALICE, &UNKNOWN_TOKEN),
        c,
        ks721::ERR_UNKNOWN_TOKEN,
        "a transfer of a token that does not exist",
    )?;
    reverted(
        nft_transfer(c, &s.holder, &ZERO_ACCOUNT, &a),
        c,
        ERR_ZERO_ACCOUNT,
        "a transfer to the zero account",
    )?;
    reverted(
        nft_transfer(c, &s.holder, c, &a),
        c,
        ERR_SELF_RECIPIENT,
        "a transfer to the collection itself",
    )?;
    reverted(
        nft_transfer(c, &s.holder, &s.holder, &a),
        c,
        ks721::ERR_ALREADY_THE_OWNER,
        "a transfer to the owner",
    )?;
    fund(&s.holder)?;
    let paid = mock::call_with_funds(s.holder, *c, "transfer", &encode(&(ALICE, a)), &funds(1));
    reverted(
        paid,
        c,
        ERR_UNEXPECTED_FUNDS,
        "a transfer with attached funds",
    )?;
    let paid = mock::call_with_funds(s.holder, *c, "owner_of", &encode(&a), &funds(1));
    reverted(paid, c, ERR_UNEXPECTED_FUNDS, "a view with attached funds")
}

fn ks721_approvals(s: &Ks721Subject) -> Outcome {
    let c = &s.collection;
    let (a, b) = first_tokens(s)?;
    reverted(
        nft_approve(c, &BOB, Some(BOB), &a),
        c,
        ks721::ERR_NOT_AUTHORIZED,
        "an approve by another account",
    )?;
    mock::clear_events();
    succeeded(nft_approve(c, &s.holder, Some(BOB), &a), "approve")?;
    let expected = [ks721::Event::Approve {
        owner: s.holder,
        spender: Some(BOB),
        token_id: a,
    }];
    let events = ks721_events(c)?;
    ensure(events == expected, || format!("approve emitted {events:?}"))?;
    ensure(approved(c, &a)? == Some(BOB), || {
        "approved() is not the spender".to_string()
    })?;
    reverted(
        nft_transfer_from(c, &BOB, &ALICE, &BOB, &a),
        c,
        ks721::ERR_WRONG_OWNER,
        "a transfer_from naming another owner",
    )?;
    reverted(
        nft_transfer_from(c, &BOB, &s.holder, &ALICE, &b),
        c,
        ks721::ERR_NOT_AUTHORIZED,
        "a transfer_from of a token the spender may not move",
    )?;
    reverted(
        nft_transfer(c, &BOB, &ALICE, &a),
        c,
        ks721::ERR_NOT_THE_OWNER,
        "a transfer by the approved account",
    )?;
    let expected = [ks721::Event::Transfer {
        from: s.holder,
        to: ALICE,
        token_id: a,
    }];
    moved(
        c,
        || nft_transfer_from(c, &BOB, &s.holder, &ALICE, &a),
        &expected,
        "transfer_from by the approved account",
    )?;
    reverted(
        nft_transfer_from(c, &BOB, &ALICE, &BOB, &a),
        c,
        ks721::ERR_NOT_AUTHORIZED,
        "a transfer_from after the change of owner cleared the approval",
    )?;
    let expected = [ks721::Event::Transfer {
        from: s.holder,
        to: BOB,
        token_id: b,
    }];
    moved(
        c,
        || nft_transfer_from(c, &s.holder, &s.holder, &BOB, &b),
        &expected,
        "transfer_from by the owner",
    )?;
    succeeded(nft_approve(c, &BOB, Some(ALICE), &b), "approve")?;
    mock::clear_events();
    succeeded(
        nft_approve(c, &BOB, None, &b),
        "an approve clearing the approval",
    )?;
    ensure(approved(c, &b)?.is_none(), || {
        "approve(None) did not clear".to_string()
    })?;
    let expected = [ks721::Event::Approve {
        owner: BOB,
        spender: None,
        token_id: b,
    }];
    let events = ks721_events(c)?;
    ensure(events == expected, || {
        format!("approve(None) emitted {events:?}")
    })
}

fn ks721_operators(s: &Ks721Subject) -> Outcome {
    let c = &s.collection;
    let (a, b) = first_tokens(s)?;
    mock::set_block_timestamp(1_000);
    reverted(
        set_operator(c, &s.holder, &ZERO_ACCOUNT, true, None),
        c,
        ERR_ZERO_ACCOUNT,
        "the zero account as an operator",
    )?;
    mock::clear_events();
    succeeded(
        set_operator(c, &s.holder, &BOB, true, Some(2_000)),
        "set_operator",
    )?;
    let expected = [ks721::Event::Operator {
        owner: s.holder,
        operator: BOB,
        approved: true,
        expires_at: Some(2_000),
    }];
    let events = ks721_events(c)?;
    ensure(events == expected, || {
        format!("set_operator emitted {events:?}")
    })?;
    let is_operator =
        || -> Result<bool, String> { view(c, "is_operator", &encode(&(s.holder, BOB))) };
    let info = || -> Result<(bool, Option<u64>), String> {
        view(c, "operator_info", &encode(&(s.holder, BOB)))
    };
    ensure(is_operator()?, || "is_operator() is false".to_string())?;
    ensure(info()? == (true, Some(2_000)), || {
        "operator_info is not (true, Some(2000))".to_string()
    })?;
    succeeded(
        nft_approve(c, &BOB, Some(ALICE), &b),
        "an approve by an operator",
    )?;
    let expected = [ks721::Event::Transfer {
        from: s.holder,
        to: ALICE,
        token_id: a,
    }];
    moved(
        c,
        || nft_transfer_from(c, &BOB, &s.holder, &ALICE, &a),
        &expected,
        "transfer_from by an operator",
    )?;
    mock::set_block_timestamp(2_000);
    ensure(!is_operator()?, || {
        "is_operator() is true at the expiry".to_string()
    })?;
    ensure(info()? == (true, Some(2_000)), || {
        "operator_info is not (true, Some(2000)) at the expiry".to_string()
    })?;
    reverted(
        nft_transfer_from(c, &BOB, &s.holder, &BOB, &b),
        c,
        ks721::ERR_NOT_AUTHORIZED,
        "a transfer_from by an expired operator",
    )?;
    succeeded(
        set_operator(c, &s.holder, &BOB, false, None),
        "a revoking set_operator",
    )?;
    ensure(!info()?.0, || {
        "operator_info still approves after the revocation".to_string()
    })
}

fn ks721_data(args: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    ks721::received(args).map(|received| received.data)
}

fn ks721_probe(collection: &[u8; 32], args: &[u8]) -> Vec<u8> {
    let token_id = decode::<([u8; 32], TokenId, Vec<u8>)>(args).map(|(_, id, _)| id);
    let owner = token_id
        .ok()
        .and_then(|id| ks721::Collection(*collection).owner_of(&id).ok().flatten());
    encode(&owner)
}

fn ks721_transfer_and_call(s: &Ks721Subject) -> Outcome {
    let c = &s.collection;
    let (a, b) = first_tokens(s)?;
    fund(&s.holder)?;
    register_receiver(ks721::ON_RECEIVED, ks721_data, ks721_probe);
    let call = |caller: [u8; 32], token_id: TokenId, data: &[u8]| {
        mock::call_with_funds(
            caller,
            *c,
            "transfer_and_call",
            &encode(&(RECEIVER, token_id, data)),
            &funds(4),
        )
    };
    reverted(
        call(s.holder, a, b"refuse"),
        &RECEIVER,
        b"refused",
        "a refused transfer_and_call",
    )?;
    ensure(
        owner_of(c, &a)? == Some(s.holder) && mock::balance(&FUNDS_ASSET, &s.holder) == 10,
        || "a refused transfer_and_call moved the token or the funds".to_string(),
    )?;
    let expected = [ks721::Event::Transfer {
        from: s.holder,
        to: RECEIVER,
        token_id: a,
    }];
    let out = moved(
        c,
        || call(s.holder, a, b"hello"),
        &expected,
        "transfer_and_call",
    )?;
    check_answer(&out, b"hello")?;
    let seen = seen()?;
    ensure(seen.caller == *c, || {
        "the receiver was not called by the collection".to_string()
    })?;
    ensure(
        seen.args == encode(&(s.holder, a, b"hello".as_slice())),
        || "the receiver was not given (from, token_id, data)".to_string(),
    )?;
    ensure(seen.probe == encode(&Some(RECEIVER)), || {
        "the token was not the receiver's before it was called".to_string()
    })?;
    check_forwarded(&seen, c, &s.holder)?;
    succeeded(nft_approve(c, &s.holder, Some(BOB), &b), "approve")?;
    let by_spender = mock::call(
        BOB,
        *c,
        "transfer_and_call",
        &encode(&(RECEIVER, b, b"".as_slice())),
    );
    reverted(
        by_spender,
        c,
        ks721::ERR_NOT_THE_OWNER,
        "a transfer_and_call by the approved account",
    )?;
    let to_account = mock::call(
        s.holder,
        *c,
        "transfer_and_call",
        &encode(&(BOB, b, b"".as_slice())),
    );
    ensure(to_account.is_err(), || {
        "transfer_and_call to an account that is no contract succeeded".to_string()
    })
}

fn ks721_burnable(s: &Ks721Subject) -> Outcome {
    let c = &s.collection;
    if !standards_of(c)?.iter().any(|id| id == ks721::BURNABLE) {
        return Ok(());
    }
    let (a, b) = first_tokens(s)?;
    reverted(
        mock::call(BOB, *c, "burn", &encode(&a)),
        c,
        ks721::ERR_NOT_AUTHORIZED,
        "a burn by another account",
    )?;
    let expected = [ks721::Event::Transfer {
        from: s.holder,
        to: ZERO_ACCOUNT,
        token_id: a,
    }];
    moved(
        c,
        || mock::call(s.holder, *c, "burn", &encode(&a)),
        &expected,
        "burn",
    )?;
    reverted(
        mock::call(s.holder, *c, "burn", &encode(&a)),
        c,
        ks721::ERR_UNKNOWN_TOKEN,
        "a burn of a burned token",
    )?;
    succeeded(nft_approve(c, &s.holder, Some(BOB), &b), "approve")?;
    let expected = [ks721::Event::Transfer {
        from: s.holder,
        to: ZERO_ACCOUNT,
        token_id: b,
    }];
    moved(
        c,
        || mock::call(BOB, *c, "burn", &encode(&b)),
        &expected,
        "a burn by the approved account",
    )?;
    Ok(())
}

fn id_set(ids: Vec<TokenId>) -> BTreeSet<TokenId> {
    ids.into_iter().collect()
}

fn ks721_enumerable(s: &Ks721Subject) -> Outcome {
    let c = &s.collection;
    if !standards_of(c)?.iter().any(|id| id == ks721::ENUMERABLE) {
        return Ok(());
    }
    let (a, _) = first_tokens(s)?;
    let check = |what: &str| -> Outcome {
        let owners = owners(&ks721_events(c)?);
        let existing: BTreeSet<TokenId> = owners
            .iter()
            .filter(|(_, owner)| owner.is_some())
            .map(|(id, _)| *id)
            .collect();
        let supply: u64 = view(c, "total_supply", &[])?;
        ensure(u64::try_from(existing.len()).ok() == Some(supply), || {
            format!(
                "{what}: total_supply() is {supply}, the events give {}",
                existing.len()
            )
        })?;
        let mut listed = Vec::new();
        let mut start: u64 = 0;
        loop {
            let page: Vec<TokenId> = view(c, "tokens", &encode(&(start, 10u32)))?;
            let len = u64::try_from(page.len()).map_err(|_| "a long page".to_string())?;
            if len == 0 {
                break;
            }
            listed.extend(page);
            start = start.checked_add(len).ok_or("too many tokens")?;
        }
        ensure(
            id_set(listed.clone()) == existing && listed.len() == existing.len(),
            || format!("{what}: tokens() does not list every token once"),
        )?;
        for owner in [s.holder, ALICE] {
            let mine: BTreeSet<TokenId> = owners
                .iter()
                .filter(|(_, o)| **o == Some(owner))
                .map(|(id, _)| *id)
                .collect();
            let page: Vec<TokenId> = view(c, "tokens_of", &encode(&(owner, 0u64, 100u32)))?;
            ensure(id_set(page) == mine, || {
                format!("{what}: tokens_of() does not list an owner's tokens")
            })?;
        }
        Ok(())
    };
    check("after the setup")?;
    succeeded(nft_transfer(c, &s.holder, &ALICE, &a), "transfer")?;
    check("after a transfer")
}

/// Checks the KS-721 collection `setup` deploys (see the module docs).
pub fn ks721(setup: impl Fn() -> Ks721Subject) -> Outcome {
    let checks: [Check<Ks721Subject>; 10] = [
        ("declaration", ks721_declaration),
        ("ownership", ks721_ownership),
        ("unknown method", ks721_unknown_method),
        ("transfer", ks721_transfer),
        ("refusals", ks721_refusals),
        ("approvals", ks721_approvals),
        ("operators", ks721_operators),
        ("transfer_and_call", ks721_transfer_and_call),
        ("KS-721-burnable", ks721_burnable),
        ("KS-721-enumerable", ks721_enumerable),
    ];
    run_checks(&setup, &checks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::CallResult;
    use crate::ks20::Ks20;
    use crate::ks721::Ks721;

    const HOLDER: [u8; 32] = [0x11; 32];
    const TOKEN: Ks20 = Ks20::new();
    const NFT: Ks721 = Ks721::new();

    fn token(method: &str, args: &[u8]) -> CallResult {
        if method == "init" {
            TOKEN.init("Token", "TKN", 0)?;
            TOKEN.mint(&crate::caller(), 1_000)?;
            return crate::ok(&());
        }
        TOKEN
            .dispatch(method, args)
            .unwrap_or_else(|| Err(ERR_UNKNOWN_METHOD.to_vec()))
    }

    fn check_token(dispatch: fn(&str, &[u8]) -> CallResult) -> Outcome {
        ks20(|| Ks20Subject {
            token: mock::deploy(HOLDER, 0, b"token", dispatch, &[]).unwrap(),
            holder: HOLDER,
        })
    }

    fn collection(method: &str, args: &[u8]) -> CallResult {
        if method == "init" {
            NFT.init("Collection", "COL")?;
            for n in 1..=2 {
                NFT.mint(&crate::caller(), &ks721::id(n), None)?;
            }
            return crate::ok(&());
        }
        NFT.dispatch(method, args)
            .unwrap_or_else(|| Err(ERR_UNKNOWN_METHOD.to_vec()))
    }

    fn check_collection(dispatch: fn(&str, &[u8]) -> CallResult) -> Outcome {
        ks721(|| Ks721Subject {
            collection: mock::deploy(HOLDER, 0, b"collection", dispatch, &[]).unwrap(),
            holder: HOLDER,
            tokens: Vec::from([ks721::id(1), ks721::id(2)]),
        })
    }

    #[test]
    fn the_kit_passes_the_reference_implementations() {
        check_token(token).unwrap();
        check_collection(collection).unwrap();
    }

    #[test]
    fn the_kit_finds_a_transfer_of_zero() {
        fn lax(method: &str, args: &[u8]) -> CallResult {
            if method == "transfer" && decode::<([u8; 32], u128)>(args).is_ok_and(|(_, a)| a == 0) {
                return crate::ok(&());
            }
            token(method, args)
        }
        let failure = check_token(lax).unwrap_err();
        assert!(
            failure.starts_with("refusals: a transfer of 0"),
            "{failure}"
        );
    }

    #[test]
    fn the_kit_finds_an_event_without_a_change() {
        fn noisy(method: &str, args: &[u8]) -> CallResult {
            let out = token(method, args)?;
            if method == "transfer" {
                ks20::Event::Transfer {
                    from: HOLDER,
                    to: BOB,
                    amount: 1,
                }
                .emit();
            }
            Ok(out)
        }
        let failure = check_token(noisy).unwrap_err();
        assert!(failure.starts_with("transfer: "), "{failure}");
    }

    #[test]
    fn the_kit_finds_a_late_declaration() {
        fn late(method: &str, args: &[u8]) -> CallResult {
            if method == "init" {
                ks20::Event::Transfer {
                    from: ZERO_ACCOUNT,
                    to: HOLDER,
                    amount: 1,
                }
                .emit();
            }
            token(method, args)
        }
        let failure = check_token(late).unwrap_err();
        assert!(failure.starts_with("declaration: "), "{failure}");
    }

    #[test]
    fn the_kit_finds_a_receiver_answer_wrapped_again() {
        fn wrapping(method: &str, args: &[u8]) -> CallResult {
            let out = token(method, args)?;
            if method == "transfer_and_call" {
                return crate::ok(&out);
            }
            Ok(out)
        }
        let failure = check_token(wrapping).unwrap_err();
        assert!(failure.starts_with("transfer_and_call: "), "{failure}");
    }

    #[test]
    fn the_kit_finds_a_transfer_by_anyone() {
        fn open(method: &str, args: &[u8]) -> CallResult {
            if method == "transfer" {
                let (to, token_id): ([u8; 32], TokenId) = decode(args)?;
                let owner = NFT
                    .owner_of(&token_id)?
                    .ok_or_else(|| ks721::ERR_UNKNOWN_TOKEN.to_vec())?;
                NFT.transfer(&owner, &to, &token_id)?;
                return crate::ok(&());
            }
            collection(method, args)
        }
        let failure = check_collection(open).unwrap_err();
        assert!(
            failure.starts_with("refusals: a transfer by another account"),
            "{failure}"
        );
    }
}
