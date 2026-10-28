//! The poll of the docs' tutorial "Your first contract" (`content/<language>/docs/first-contract.md` of ksana-site,
//! `contracts/examples/poll`, D-124), compiled to wasm by `ksana-test-contracts` and used as the tutorial uses it on
//! the signet, each step an op of its own: deployed with a question and its options, voted on, voted on again, read,
//! closed by someone other than its creator and then by its creator. Its native tests run it on the contract SDK's mock
//! host; this test runs its module in the real VM, through the deployment checks and metered, with the execution
//! layer's fees and receipts.

mod common;

use std::collections::BTreeMap;

use borsh::BorshSerialize;
use common::*;
use ksana_execution::builtin::contract_id;
use ksana_execution::{OpStatus, Receipt};
use ksana_primitives::hash::sha256;
use ksana_primitives::Action;
use ksana_test_contracts::POLL;

const GAS: u64 = 5_000_000;
const FEE: u128 = 7;

fn args<T: BorshSerialize>(v: &T) -> Vec<u8> {
    borsh::to_vec(v).unwrap()
}

/// A chain executing every op as a block of its own, as the tutorial sends its operations one after another.
struct World {
    chain: Chain,
    nonces: BTreeMap<[u8; 32], u64>,
}

impl World {
    fn send(&mut self, u: &User, action: Action) -> Receipt {
        let nonce = self.nonces.entry(u.account).or_insert(0);
        let entry = sponsored(u, *nonce, FEE, GAS, action);
        *nonce += 1;
        let mut out = self.chain.block(vec![entry]).expect("valid block");
        assert_eq!(out.receipts.len(), 1);
        out.receipts.remove(0)
    }

    /// Calls `method` of `contract` with Borsh-encoded `a` as `u`.
    fn call<T: BorshSerialize>(
        &mut self,
        u: &User,
        contract: [u8; 32],
        method: &str,
        a: &T,
    ) -> Receipt {
        self.send(
            u,
            Action::Call {
                contract,
                method: method.into(),
                args: args(a),
                funds: Vec::new(),
            },
        )
    }

    /// A view's result, decoded.
    fn view<R: borsh::BorshDeserialize, T: BorshSerialize>(
        &mut self,
        contract: [u8; 32],
        method: &str,
        a: &T,
    ) -> R {
        let r = self.call(&user("viewer"), contract, method, a);
        assert_eq!(r.status, OpStatus::Success, "{method}: {r:?}");
        borsh::from_slice(&r.output).unwrap()
    }
}

fn ok(r: &Receipt) {
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
}

fn failed_with(r: &Receipt, reason: &str) {
    assert!(
        matches!(&r.status, OpStatus::Failed(why) if why.contains(reason)),
        "expected {reason}: {r:?}"
    );
    assert!(r.events.is_empty(), "{r:?}");
}

/// The events of a receipt, as `(contract, data)`.
fn events_of(r: &Receipt) -> Vec<([u8; 32], Vec<u8>)> {
    r.events
        .iter()
        .map(|e| (e.contract, e.data.clone()))
        .collect()
}

#[test]
fn the_tutorials_poll_runs_as_the_tutorial_says() {
    let (creator, alice, bob) = (user("creator"), user("alice"), user("bob"));
    let mut w = World {
        chain: Chain::new(
            [&creator, &alice, &bob, &user("viewer")]
                .iter()
                .map(|u| (u.account, 1_000_000))
                .collect(),
        ),
        nonces: BTreeMap::new(),
    };

    // Deploy it: `init`'s arguments are the question and the options.
    let poll = contract_id(&creator.account, 0, &sha256(POLL));
    let question = "Which fruit?".to_string();
    let options = vec!["apple".to_string(), "pear".to_string(), "plum".to_string()];
    let r = w.send(
        &creator,
        Action::Deploy {
            code: POLL.to_vec(),
            init_args: args(&(question.clone(), options.clone())),
            funds: Vec::new(),
        },
    );
    ok(&r);
    let state: (String, Vec<String>, bool) = w.view(poll, "poll", &());
    assert_eq!(state, (question.clone(), options.clone(), true));

    // Vote: the option's votes so far, with the `voted` event.
    let r = w.call(&alice, poll, "vote", &1u32);
    ok(&r);
    assert_eq!(r.output, 1u64.to_le_bytes());
    assert_eq!(
        events_of(&r),
        [(poll, args(&("voted".to_string(), alice.account, 1u32)))]
    );

    // Try to vote again: refused, and the fee is paid all the same.
    let before = w.chain.ksa(&alice.account);
    failed_with(&w.call(&alice, poll, "vote", &1u32), "already voted");
    assert_eq!(w.chain.ksa(&alice.account), before - FEE);
    failed_with(&w.call(&bob, poll, "vote", &3u32), "unknown option");
    ok(&w.call(&bob, poll, "vote", &1u32));

    // Read the results.
    assert_eq!(w.view::<Vec<u64>, _>(poll, "results", &()), [0, 2, 0]);
    assert_eq!(
        w.view::<Option<u32>, _>(poll, "voted", &alice.account),
        Some(1)
    );
    assert_eq!(
        w.view::<Option<u32>, _>(poll, "voted", &creator.account),
        None
    );

    // Close the poll: only its creator can.
    failed_with(&w.call(&bob, poll, "close", &()), "only the creator");
    let r = w.call(&creator, poll, "close", &());
    ok(&r);
    assert_eq!(events_of(&r), [(poll, args(&("closed".to_string(), 2u64)))]);
    failed_with(&w.call(&creator, poll, "vote", &0u32), "voting is closed");
    let state: (String, Vec<String>, bool) = w.view(poll, "poll", &());
    assert_eq!(state, (question, options, false));
    assert_eq!(w.view::<Vec<u64>, _>(poll, "results", &()), [0, 2, 0]);
}

#[test]
fn the_tutorials_poll_refuses_a_poll_it_cannot_hold() {
    let creator = user("creator");
    let mut w = World {
        chain: Chain::new(vec![(creator.account, 1_000_000)]),
        nonces: BTreeMap::new(),
    };
    for (question, options, reason) in [
        ("", vec!["yes", "no"], "the question must be 1 to 200 bytes"),
        ("Which fruit?", vec!["apple"], "a poll has 2 to 8 options"),
        (
            "Which fruit?",
            vec!["apple", ""],
            "an option must be 1 to 64 bytes",
        ),
    ] {
        let options: Vec<String> = options.into_iter().map(String::from).collect();
        let r = w.send(
            &creator,
            Action::Deploy {
                code: POLL.to_vec(),
                init_args: args(&(question.to_string(), options)),
                funds: Vec::new(),
            },
        );
        failed_with(&r, reason);
    }
}
