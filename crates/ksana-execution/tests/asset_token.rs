//! The example contract `asset-token` (`contracts/examples/asset-token`) on the real VM: its supply, issued at
//! deployment, is a built-in asset with the metadata `ksana_getAsset` reads, and it trades against KSA on the `amm`
//! example with attached funds (D-073), the flow of the Wizards demo on the public signet; then a deposit with a minimum
//! of shares.

mod common;

use common::probe::*;
use common::*;
use ksana_execution::builtin::contract_id;
use ksana_execution::keys::{self, AssetMeta};
use ksana_execution::OpStatus;
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Funds};
use ksana_test_contracts::{AMM, ASSET_TOKEN};

/// The asset-token example's only `sub_id`.
const SUB_ID: [u8; 32] = [0u8; 32];

fn deploy(code: &[u8], init_args: Vec<u8>) -> Action {
    Action::Deploy {
        code: code.to_vec(),
        init_args,
        funds: vec![],
    }
}

/// The AMM's swap output with its 0.3% fee.
fn swap_output(amount_in: u128, reserve_in: u128, reserve_out: u128) -> u128 {
    amount_in * 997 * reserve_out / (reserve_in * 1000 + amount_in * 997)
}

#[test]
fn an_issued_asset_trades_against_ksa_on_the_amm() {
    let alice = user("alice");
    let bob = user("bob");
    let mut w = World::new(&[&alice, &bob]);

    // Alice deploys the token: the whole supply is hers, as a built-in asset
    let supply = 21_000_000u128 * 100_000_000;
    let r = w.run(
        &alice,
        deploy(
            ASSET_TOKEN,
            enc(&("Wizards".to_string(), "WIZ".to_string(), 8u8, supply)),
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let token = contract_id(&alice.account, 0, &sha256(ASSET_TOKEN));
    let wiz = sha256(&[token, SUB_ID].concat());
    assert_eq!(w.balance(&wiz, &alice.account), supply);
    assert_eq!(
        w.chain.get::<AssetMeta>(&keys::asset(&wiz)),
        Some(AssetMeta {
            name: "Wizards".into(),
            symbol: "WIZ".into(),
            decimals: 8,
            total_supply: supply,
            issuer: token,
            // created without capabilities: the protocol keeps the supply fixed (D-121)
            capabilities: 0,
            paused: false,
        })
    );
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].contract, token);
    assert_eq!(
        r.events[0].data,
        enc(&("issue", wiz, alice.account, supply))
    );
    let r = w.run(&bob, call(token, "asset_id", vec![], vec![]));
    assert_eq!(r.output, wiz.to_vec());

    // The pool: side a is the token, side b KSA (`Side::Asset` is variant 1)
    let r = w.run(&alice, deploy(AMM, enc(&(1u8, wiz, 1u8, KSA))));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let pool = contract_id(&alice.account, 1, &sha256(AMM));

    // Alice adds liquidity, attaching both sides (KSA's ID is the smallest, so it comes first)
    let (wiz_in, ksa_in) = (80_000_000_000u128, 400_000u128);
    let r = w.run(
        &alice,
        call(
            pool,
            "add_liquidity",
            enc(&(wiz_in, ksa_in)),
            vec![
                ksa(ksa_in),
                Funds {
                    asset: wiz,
                    amount: wiz_in,
                },
            ],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(w.balance(&wiz, &pool), wiz_in);
    assert_eq!(w.chain.ksa(&pool), ksa_in);

    // Bob swaps KSA for the token, which arrives as a plain balance of the asset
    let amount = 50_000u128;
    let expected = swap_output(amount, ksa_in, wiz_in);
    let r = w.run(
        &bob,
        call(
            pool,
            "swap",
            enc(&(KSA, amount, expected)),
            vec![ksa(amount)],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(r.output, enc(&expected));
    assert_eq!(w.balance(&wiz, &bob.account), expected);
    assert_eq!(w.chain.ksa(&bob.account), START - amount - 2 * FEE);
    assert_eq!(w.balance(&wiz, &pool), wiz_in - expected);
    assert_eq!(w.chain.ksa(&pool), ksa_in + amount);
    let r = w.run(&bob, call(pool, "reserves", vec![], vec![]));
    assert_eq!(r.output, enc(&(wiz_in - expected, ksa_in + amount)));
    // Nothing more was issued
    assert_eq!(
        w.chain
            .get::<AssetMeta>(&keys::asset(&wiz))
            .map(|m| m.total_supply),
        Some(supply)
    );

    // Bob adds liquidity in the pool's ratio with a minimum of shares: one share more than the deposit gets fails it,
    // and the funds he attached stay his
    let (reserve_wiz, reserve_ksa) = (wiz_in - expected, ksa_in + amount);
    let r = w.run(&bob, call(pool, "total_shares", vec![], vec![]));
    let total = u128::from_le_bytes(r.output.as_slice().try_into().unwrap());
    let wiz_add = expected / 2;
    let ksa_add = (wiz_add * reserve_ksa).div_ceil(reserve_wiz);
    let shares = wiz_add * total / reserve_wiz;
    let add = |min_shares: u128| {
        call(
            pool,
            "add_liquidity",
            enc(&(wiz_add, ksa_add, min_shares)),
            vec![
                ksa(ksa_add),
                Funds {
                    asset: wiz,
                    amount: wiz_add,
                },
            ],
        )
    };
    let ksa_before = w.chain.ksa(&bob.account);
    let r = w.run(&bob, add(shares + 1));
    assert_eq!(failed(&r), "revert: shares below min_shares");
    assert_eq!(w.balance(&wiz, &bob.account), expected);
    assert_eq!(w.chain.ksa(&bob.account), ksa_before - FEE);
    // At exactly that many shares it goes through, taking what the ratio needs
    let r = w.run(&bob, add(shares));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(r.output, enc(&shares));
    let (used_wiz, used_ksa) = (
        (shares * reserve_wiz).div_ceil(total),
        (shares * reserve_ksa).div_ceil(total),
    );
    assert_eq!(w.balance(&wiz, &bob.account), expected - used_wiz);
    assert_eq!(w.chain.ksa(&bob.account), ksa_before - 2 * FEE - used_ksa);
    assert_eq!(w.balance(&wiz, &pool), reserve_wiz + used_wiz);
    assert_eq!(w.chain.ksa(&pool), reserve_ksa + used_ksa);
}

#[test]
fn a_zero_supply_fails_the_deployment() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let r = w.run(
        &alice,
        deploy(
            ASSET_TOKEN,
            enc(&("Wizards".to_string(), "WIZ".to_string(), 8u8, 0u128)),
        ),
    );
    assert!(failed(&r).contains("supply must be positive"), "{r:?}");
    let token = contract_id(&alice.account, 0, &sha256(ASSET_TOKEN));
    let wiz = sha256(&[token, SUB_ID].concat());
    assert_eq!(w.chain.get::<AssetMeta>(&keys::asset(&wiz)), None);
}
