//! Property test of the issuer capabilities of built-in assets (D-121): random sequences of the Asset module's
//! operations (creating with random capabilities, issuing, transferring, which the funds attached to a call are too,
//! burning, freezing, allowing, pausing and renouncing, by the issuing contract or by another one) keep the module's
//! invariants after every step:
//!
//! - the balances of an asset sum to its total supply;
//! - once an asset exists its capabilities never gain a bit, its issuer, name, symbol and decimals never change, and
//!   `paused` implies `pause`;
//! - a refused step writes nothing, and the executor discards it as it discards a failed op;
//! - a transfer succeeds exactly when the RPC's `can_send` and `can_receive` allow its two parties and the balance
//!   suffices, a burn exactly when `can_send` allows the holder and the balance suffices, and a refusal by the asset's
//!   rules says `rejected:`.

use std::sync::Arc;

use ksana_execution::builtin::{self, Fault};
use ksana_execution::keys::AssetMeta;
use ksana_execution::{MemState, Overlay};
use ksana_primitives::asset::{CAP_PAUSE, DEFAULT_CAPABILITIES, STATUS_ALLOWED, STATUS_FROZEN};
use ksana_vm::AssetMetaInput;
use proptest::prelude::*;

/// The accounts: the two issuing contracts first, then three holders.
const ACCOUNTS: [[u8; 32]; 5] = [[1; 32], [2; 32], [0xA1; 32], [0xB0; 32], [0xC0; 32]];
/// The number of assets: one for each issuing contract.
const ASSETS: usize = 2;

fn asset(i: usize) -> [u8; 32] {
    builtin::asset_id(&ACCOUNTS[i], &[9; 32])
}

fn meta(i: usize) -> AssetMetaInput {
    AssetMetaInput {
        name: format!("Asset {i}"),
        symbol: format!("A{i}"),
        decimals: 6,
    }
}

#[derive(Clone, Debug)]
enum Step {
    /// The asset's issuing contract creates it with `caps` (bit 0x10 is undefined).
    Create {
        asset: usize,
        caps: u8,
        to: usize,
        amount: u128,
    },
    /// `by` issues the asset with its metadata; only its issuing contract may.
    Issue {
        asset: usize,
        by: usize,
        to: usize,
        amount: u128,
    },
    Transfer {
        asset: usize,
        from: usize,
        to: usize,
        amount: u128,
    },
    Burn {
        asset: usize,
        from: usize,
        amount: u128,
    },
    /// `by` sets or clears `flag` of `account` (4 is an undefined flag).
    Status {
        asset: usize,
        by: usize,
        account: usize,
        flag: u8,
        on: bool,
    },
    Pause {
        asset: usize,
        by: usize,
        on: bool,
    },
    Renounce {
        asset: usize,
        by: usize,
        caps: u8,
    },
}

fn amount() -> impl Strategy<Value = u128> {
    prop_oneof![9 => 0u128..400, 1 => Just(u128::MAX)]
}

fn step() -> impl Strategy<Value = Step> {
    let asset = 0..ASSETS;
    let account = 0..ACCOUNTS.len();
    prop_oneof![
        1 => (asset.clone(), 0u8..0x20, account.clone(), amount())
            .prop_map(|(asset, caps, to, amount)| Step::Create { asset, caps, to, amount }),
        2 => (asset.clone(), account.clone(), account.clone(), amount())
            .prop_map(|(asset, by, to, amount)| Step::Issue { asset, by, to, amount }),
        6 => (asset.clone(), account.clone(), account.clone(), amount())
            .prop_map(|(asset, from, to, amount)| Step::Transfer { asset, from, to, amount }),
        2 => (asset.clone(), account.clone(), amount())
            .prop_map(|(asset, from, amount)| Step::Burn { asset, from, amount }),
        4 => (
            asset.clone(),
            account.clone(),
            account.clone(),
            prop_oneof![Just(STATUS_FROZEN), Just(STATUS_ALLOWED), Just(4u8)],
            any::<bool>(),
        )
            .prop_map(|(asset, by, account, flag, on)| Step::Status { asset, by, account, flag, on }),
        2 => (asset.clone(), account.clone(), any::<bool>())
            .prop_map(|(asset, by, on)| Step::Pause { asset, by, on }),
        1 => (asset, account, 0u8..0x20)
            .prop_map(|(asset, by, caps)| Step::Renounce { asset, by, caps }),
    ]
}

/// What the module holds for the two assets: each asset's record, and each account's balance and status flags.
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    metas: Vec<Option<AssetMeta>>,
    balances: Vec<Vec<u128>>,
    statuses: Vec<Vec<u8>>,
}

fn view(ov: &Overlay) -> View {
    let assets = 0..ASSETS;
    View {
        metas: assets
            .clone()
            .map(|i| builtin::asset_meta(ov, &asset(i)).unwrap())
            .collect(),
        balances: assets
            .clone()
            .map(|i| {
                ACCOUNTS
                    .iter()
                    .map(|a| builtin::balance(ov, &asset(i), a).unwrap())
                    .collect()
            })
            .collect(),
        statuses: assets
            .map(|i| {
                ACCOUNTS
                    .iter()
                    .map(|a| builtin::asset_status(ov, &asset(i), a).unwrap())
                    .collect()
            })
            .collect(),
    }
}

fn apply(ov: &mut Overlay, step: &Step) -> Result<(), Fault> {
    match *step {
        Step::Create {
            asset: i,
            caps,
            to,
            amount,
        } => builtin::create(
            ov,
            &asset(i),
            &ACCOUNTS[i],
            &meta(i),
            caps,
            &ACCOUNTS[to],
            amount,
        ),
        Step::Issue {
            asset: i,
            by,
            to,
            amount,
        } => builtin::issue(
            ov,
            &asset(i),
            &ACCOUNTS[by],
            &meta(i),
            &ACCOUNTS[to],
            amount,
        ),
        Step::Transfer {
            asset: i,
            from,
            to,
            amount,
        } => builtin::transfer(ov, &asset(i), &ACCOUNTS[from], &ACCOUNTS[to], amount),
        Step::Burn {
            asset: i,
            from,
            amount,
        } => builtin::burn(ov, &asset(i), &ACCOUNTS[from], amount),
        Step::Status {
            asset: i,
            by,
            account,
            flag,
            on,
        } => builtin::set_status(ov, &asset(i), &ACCOUNTS[by], &ACCOUNTS[account], flag, on),
        Step::Pause { asset: i, by, on } => builtin::set_paused(ov, &asset(i), &ACCOUNTS[by], on),
        Step::Renounce { asset: i, by, caps } => {
            builtin::renounce(ov, &asset(i), &ACCOUNTS[by], caps)
        }
    }
}

/// Whether a transfer (`to` is `Some`) or a burn should succeed, from the view before it, and whether only the asset's
/// rules stand in its way.
fn expected_movement(
    before: &View,
    i: usize,
    from: usize,
    to: Option<usize>,
    amount: u128,
) -> (bool, bool) {
    let Some(m) = &before.metas[i] else {
        return (false, false);
    };
    let status = &before.statuses[i];
    let allowed = builtin::can_send(m, &ACCOUNTS[from], status[from])
        && to.is_none_or(|to| builtin::can_receive(m, &ACCOUNTS[to], status[to]));
    let balances = &before.balances[i];
    let funded = balances[from] >= amount
        && to.is_none_or(|to| to == from || balances[to].checked_add(amount).is_some());
    (allowed && funded, !allowed)
}

fn check_invariants(before: &View, after: &View, step: &Step) -> Result<(), TestCaseError> {
    // asset i's issuing contract is the account i
    for (i, issuer) in ACCOUNTS.iter().take(ASSETS).enumerate() {
        let balances = after.balances[i]
            .iter()
            .try_fold(0u128, |sum, b| sum.checked_add(*b));
        match (&before.metas[i], &after.metas[i]) {
            (_, None) => prop_assert_eq!(balances, Some(0), "asset {} does not exist", i),
            (old, Some(m)) => {
                prop_assert_eq!(balances, Some(m.total_supply), "asset {}", i);
                prop_assert!(!m.paused || m.capabilities & CAP_PAUSE != 0, "{:?}", m);
                prop_assert_eq!(&m.issuer, issuer);
                match old {
                    Some(o) => {
                        prop_assert_eq!(
                            m.capabilities & !o.capabilities,
                            0,
                            "gained a bit: {:?}",
                            step
                        );
                        prop_assert_eq!(
                            (&m.name, &m.symbol, m.decimals),
                            (&o.name, &o.symbol, o.decimals)
                        );
                    }
                    None => {
                        let declared = match *step {
                            Step::Create { caps, .. } => caps,
                            _ => DEFAULT_CAPABILITIES,
                        };
                        prop_assert_eq!(m.capabilities, declared, "{:?}", step);
                        prop_assert!(!m.paused);
                    }
                }
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn random_sequences_keep_the_asset_invariants(steps in prop::collection::vec(step(), 1..60)) {
        let mut ov = Overlay::new(Arc::new(MemState::default()));
        for step in &steps {
            let before = view(&ov);
            // The host derives an asset's ID from the calling contract, so no other contract can issue an asset that
            // does not exist yet; the Asset module's check of the issuer guards an asset that exists
            if let Step::Issue { asset, by, .. } = *step {
                if by != asset && before.metas[asset].is_none() {
                    continue;
                }
            }
            let result = apply(&mut ov, step);
            let after = view(&ov);
            let movement = match *step {
                Step::Transfer { asset, from, to, amount } => {
                    Some(expected_movement(&before, asset, from, Some(to), amount))
                }
                Step::Burn { asset, from, amount } => Some(expected_movement(&before, asset, from, None, amount)),
                _ => None,
            };
            match result {
                Ok(()) => {
                    if let Some((succeeds, _)) = movement {
                        prop_assert!(succeeds, "{:?} succeeded against {:?}", step, before);
                    }
                    check_invariants(&before, &after, step)?;
                    ov.commit_op();
                }
                Err(Fault::Failed(message)) => {
                    prop_assert_eq!(&after, &before, "{:?} was refused ({}) but wrote", step, message);
                    if let Some((succeeds, by_rules)) = movement {
                        prop_assert!(!succeeds, "{:?} was refused ({}) against {:?}", step, message, before);
                        if by_rules {
                            prop_assert!(message.starts_with("rejected:"), "{:?}: {}", step, message);
                        }
                    }
                    ov.discard_op();
                }
                Err(other) => prop_assert!(false, "{:?} faulted: {:?}", step, other),
            }
        }
    }
}
