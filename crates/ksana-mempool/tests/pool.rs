//! Entry pool: admission checks, replacement, capacity, the order of packing candidates and cleanup after a block.

use std::collections::BTreeMap;
use std::sync::Arc;

use ksana_crypto::{account_of, keypair_from_secret, sign_native_vmop, sign_vmop, Keypair};
use ksana_execution::keys::{self, AccountState};
use ksana_execution::{GenesisConfig, MemState, StateSource};
use ksana_mempool::{fee_rate, op_cost, AdmitError, Mempool, Policy};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, FeeMode, Funds, OutPoint, Rules, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};

struct User {
    kp: Keypair,
    account: [u8; 32],
}

fn user(seed: &str) -> User {
    let kp = keypair_from_secret(&sha256(seed.as_bytes())).unwrap();
    User {
        account: account_of(&kp),
        kp,
    }
}

fn op_with(u: &User, nonce: u64, fee: FeeMode, chain_id: u32) -> VmOp {
    op_with_gas(u, nonce, fee, chain_id, 100_000)
}

fn op_with_gas(u: &User, nonce: u64, fee: FeeMode, chain_id: u32, max_gas: u64) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id,
        account: u.account,
        nonce,
        fee,
        max_gas,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: [9; 32],
            amount: 1,
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = match op.fee {
        FeeMode::Token { .. } => sign_vmop(&op, &u.kp, &[0; 32]).unwrap(),
        // A path-C signature binds the first input of the carrying transaction (D-086); the entry pool rejects it by
        // fee mode, regardless of whether the signature is valid
        FeeMode::BtcNative => {
            let first_input = OutPoint {
                txid: [1; 32],
                vout: 0,
            };
            sign_native_vmop(&op, &first_input, &u.kp, &[0; 32]).unwrap()
        }
    };
    op
}

fn op(u: &User, nonce: u64, fee: u128) -> VmOp {
    op_with(u, nonce, FeeMode::Token { amount: fee }, DEVNET.chain_id)
}

/// A path-B op with the given action.
fn op_doing(u: &User, nonce: u64, fee: u128, action: Action) -> VmOp {
    let mut op = op(u, nonce, fee);
    op.action = action;
    op.sig = sign_vmop(&op, &u.kp, &[0; 32]).unwrap();
    op
}

/// A path-B contract call whose arguments make the encoded op about `len` bytes long.
fn big_op(u: &User, nonce: u64, fee: u128, len: usize) -> VmOp {
    let call = Action::Call {
        contract: [7; 32],
        method: "spam".into(),
        args: vec![0; len],
        funds: Vec::new(),
    };
    op_doing(u, nonce, fee, call)
}

/// Total encoded bytes of the ops in the pool.
fn pool_bytes(p: &Mempool) -> usize {
    p.pending(usize::MAX)
        .iter()
        .map(|o| o.encode().unwrap().len())
        .sum()
}

fn state(allocations: &[(&User, u128)]) -> MemState {
    let mut st = MemState::default();
    let allocations = allocations.iter().map(|(u, a)| (u.account, *a)).collect();
    st.apply(&GenesisConfig::devnet(allocations).writes().unwrap());
    st
}

fn set_account(st: &mut MemState, u: &User, nonce: u64) {
    st.0.insert(
        keys::account(&u.account),
        borsh::to_vec(&AccountState { nonce }).unwrap(),
    );
}

fn set_balance(st: &mut MemState, u: &User, amount: u128) {
    let key = keys::balance(&KSA_ASSET_ID, &u.account);
    if amount == 0 {
        st.0.remove(&key);
    } else {
        st.0.insert(key, borsh::to_vec(&amount).unwrap());
    }
}

fn arc(st: &MemState) -> Arc<dyn StateSource> {
    Arc::new(st.clone())
}

fn pool() -> Mempool {
    Mempool::new(
        DEVNET,
        Policy {
            min_token_fee: 2,
            min_relay_token_fee: 2,
            max_ops: 6,
            max_ops_per_account: 4,
            ..Policy::default()
        },
    )
}

#[test]
fn admission_checks() {
    let (a, b) = (user("a"), user("b"));
    let mut st = state(&[(&a, 100), (&b, 100)]);
    set_account(&mut st, &b, 3);
    let s = arc(&st);
    let mut p = pool();
    let invalid = |r: Result<[u8; 32], AdmitError>| matches!(r, Err(AdmitError::Invalid(_)));
    let policy = |r: Result<[u8; 32], AdmitError>| matches!(r, Err(AdmitError::Policy(_)));

    let id = p.insert(op(&a, 0, 5), &s).unwrap();
    assert_eq!(id, op(&a, 0, 5).op_id().unwrap());
    assert_eq!(
        p.insert(op(&a, 0, 5), &s),
        Ok(id),
        "resubmitting the same op"
    );
    assert_eq!(p.len(), 1);
    let gen = p.generation();

    assert!(invalid(
        p.insert(op_with(&a, 1, FeeMode::Token { amount: 5 }, 7), &s)
    ));
    assert!(invalid(p.insert(
        op_with(&a, 1, FeeMode::BtcNative, DEVNET.chain_id),
        &s
    )));
    let mut forged = op(&a, 1, 5);
    forged.nonce = 2;
    assert!(invalid(p.insert(forged, &s)), "invalid signature");
    assert!(
        policy(p.insert(op(&a, 1, 1), &s)),
        "below the minimum sponsored fee"
    );
    assert!(invalid(p.insert(op(&b, 2, 5), &s)), "nonce already used");
    assert!(
        policy(p.insert(op(&b, 7, 5), &s)),
        "nonce runs too far ahead"
    );
    // The balance of 100 must cover the sum of the sponsored fees of the queued ops
    assert!(p.insert(op(&a, 1, 90), &s).is_ok());
    assert!(invalid(p.insert(op(&a, 2, 6), &s)), "5 + 90 + 6 > 100");
    assert_eq!(p.len(), 2);
    assert!(p.generation() > gen);
}

#[test]
fn replacement_and_capacity() {
    let (a, b) = (user("a"), user("b"));
    let s = arc(&state(&[(&a, 1_000), (&b, 1_000)]));
    let mut p = pool();
    let first = p.insert(op(&a, 0, 5), &s).unwrap();
    assert!(matches!(p.insert(op(&a, 0, 5), &s), Ok(id) if id == first));
    let mut same_fee = op(&a, 0, 5);
    same_fee.max_gas = 99_999;
    same_fee.sig = sign_vmop(&same_fee, &a.kp, &[0; 32]).unwrap();
    assert!(matches!(p.insert(same_fee, &s), Err(AdmitError::Policy(_))));
    // The fee goes up by the relay floor (2) at least
    assert!(matches!(
        p.insert(op(&a, 0, 6), &s),
        Err(AdmitError::Policy(_))
    ));
    let second = p.insert(op(&a, 0, 7), &s).unwrap();
    assert!(!p.contains(&first) && p.contains(&second));
    assert_eq!(p.len(), 1);

    // At most 4 per account, at most 6 in the whole pool
    for n in 1..4 {
        p.insert(op(&a, n, 5), &s).unwrap();
    }
    assert!(matches!(
        p.insert(op(&a, 4, 5), &s),
        Err(AdmitError::Policy(_))
    ));
    p.insert(op(&b, 0, 5), &s).unwrap();
    p.insert(op(&b, 1, 5), &s).unwrap();
    assert!(matches!(
        p.insert(op(&b, 2, 5), &s),
        Err(AdmitError::Policy(_))
    ));
    assert_eq!(p.len(), 6);
}

/// A replacement raises the sponsored fee by a tenth of it, rounded up, and by the relay floor at least (D-130): one op
/// is not replaced over and over for one more unit each (A2-08), and how many times it can be replaced at all is bounded
/// by the account's balance.
#[test]
fn a_replacement_raises_the_fee_by_a_tenth_and_by_the_relay_floor() {
    let policy = Policy::default();
    assert_eq!(policy.relay_floor(), 1);
    assert_eq!(policy.replacement_fee(0), 1);
    assert_eq!(policy.replacement_fee(1), 2);
    assert_eq!(policy.replacement_fee(10), 11);
    assert_eq!(policy.replacement_fee(11), 13);
    assert_eq!(policy.replacement_fee(1_000), 1_100);
    assert_eq!(policy.replacement_fee(u128::MAX), u128::MAX);
    let floor = Policy {
        min_token_fee: 5,
        min_relay_token_fee: 3,
        ..Policy::default()
    };
    assert_eq!(floor.replacement_fee(20), 23);
    assert_eq!(floor.replacement_fee(40), 44);

    let a = user("a");
    let s = arc(&state(&[(&a, 10_000)]));
    let mut p = pool();
    let first = p.insert(op(&a, 0, 100), &s).unwrap();
    assert_eq!(
        p.insert(op(&a, 0, 109), &s),
        Err(AdmitError::Policy(
            "an op with the same nonce already exists; replacing it needs a sponsored fee of at least 110"
                .into()
        ))
    );
    let second = p.insert(op(&a, 0, 110), &s).unwrap();
    assert!(!p.contains(&first) && p.contains(&second));

    // From the least fee, each replacement paying the least it may: the balance runs out after 72 of them (at one more
    // unit each, after 9 888)
    let (mut fee, mut replaced) = (2, 0);
    p.insert(op(&a, 1, fee), &s).unwrap();
    loop {
        let next = p.policy().replacement_fee(fee);
        match p.insert(op(&a, 1, next), &s) {
            Ok(_) => (fee, replaced) = (next, replaced + 1),
            Err(AdmitError::Invalid(_)) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(replaced, 72);
    assert!(110 + fee <= 10_000 && 110 + p.policy().replacement_fee(fee) > 10_000);
}

/// The generation moves only for a change that can alter the packing candidates (D-130, A2-08): ops behind a nonce
/// gap, or after one below the packing floor, are admitted and replaced without the producer rebuilding its template
/// for them; an op filling the gap, an op meeting the floor at last, or a change in an account's run of candidates
/// moves it.
#[test]
fn only_a_change_to_the_packing_candidates_moves_the_generation() {
    let (a, b) = (user("a"), user("b"));
    let s = arc(&state(&[(&a, 1_000_000), (&b, 1_000)]));
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            min_token_fee: 3,
            min_relay_token_fee: 1,
            ..Policy::default()
        },
    );
    let candidates = |p: &Mempool| p.candidates(&s).unwrap();

    // a's nonce 0 is missing: a1 and a2 are behind a gap, however often they are replaced
    let gen = p.generation();
    p.insert(op(&a, 1, 10), &s).unwrap();
    p.insert(op(&a, 2, 10), &s).unwrap();
    let mut fee = 10;
    for _ in 0..20 {
        fee = p.policy().replacement_fee(fee);
        p.insert(op(&a, 1, fee), &s).unwrap();
    }
    assert_eq!(p.generation(), gen);
    assert!(candidates(&p).is_empty());

    // a0 fills the gap: the three of them are candidates, and replacing one of them counts
    p.insert(op(&a, 0, 10), &s).unwrap();
    assert!(p.generation() > gen);
    assert_eq!(candidates(&p).len(), 3);
    let gen = p.generation();
    p.insert(op(&a, 2, 11), &s).unwrap();
    assert!(p.generation() > gen);

    // b0 is below this node's packing floor (3) but relayed, so b1 after it is no candidate either
    let gen = p.generation();
    p.insert(op(&b, 0, 2), &s).unwrap();
    p.insert(op(&b, 1, 5), &s).unwrap();
    p.insert(op(&b, 1, 6), &s).unwrap();
    assert_eq!(p.generation(), gen);
    // ... until b0 meets the floor
    p.insert(op(&b, 0, 3), &s).unwrap();
    assert!(p.generation() > gen);
    let of_b = |p: &Mempool| {
        candidates(p)
            .iter()
            .filter(|o| o.account == b.account)
            .count()
    };
    assert_eq!(of_b(&p), 2);

    // The node marks a change when the tip moved after it read the state an op was admitted against
    let (gen, before) = (p.generation(), candidates(&p));
    p.mark_changed();
    assert_eq!((p.generation(), candidates(&p)), (gen + 1, before));
}

/// An eviction moves the generation when it takes a packing candidate, also of another account than the newcomer's
/// (D-130): an op behind a gap taking the place of another one changes nothing a template holds, while an op below the
/// packing floor taking the place of a candidate that may be left unfunded does.
#[test]
fn an_eviction_moves_the_generation_when_it_takes_a_candidate() {
    let (a, g, h) = (user("a"), user("g"), user("h"));
    let s = arc(&state(&[(&a, 1_000), (&g, 1_000), (&h, 1_000)]));
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            max_ops: 2,
            ..Policy::default()
        },
    );
    p.insert(op(&a, 0, 5), &s).unwrap();
    // g's and h's nonce 0 are missing
    let g1 = p.insert(op(&g, 1, 5), &s).unwrap();
    let (gen, before) = (p.generation(), p.candidates(&s).unwrap());
    let h1 = p.insert(op(&h, 1, 6), &s).unwrap();
    assert!(!p.contains(&g1) && p.contains(&h1));
    assert_eq!((p.generation(), p.candidates(&s).unwrap()), (gen, before));

    // w1 may be left unfunded by w0's transfer: w stands below u, whose op is below the packing floor (3)
    let (w, v, u) = (user("w"), user("v"), user("u"));
    let s = arc(&state(&[(&w, 10), (&v, 1_000), (&u, 1_000)]));
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            min_token_fee: 3,
            min_relay_token_fee: 1,
            max_ops: 3,
            ..Policy::default()
        },
    );
    let transfer = Action::AssetTransfer {
        asset: KSA_ASSET_ID,
        to: v.account,
        amount: 6,
    };
    p.insert(op_doing(&w, 0, 3, transfer), &s).unwrap();
    let w1 = p.insert(op(&w, 1, 3), &s).unwrap();
    p.insert(op(&v, 0, 5), &s).unwrap();
    assert_eq!(p.candidates(&s).unwrap().len(), 3);
    let gen = p.generation();
    let u0 = p.insert(op(&u, 0, 2), &s).unwrap();
    assert!(p.contains(&u0) && !p.contains(&w1));
    assert!(p.generation() > gen);
    assert_eq!(p.candidates(&s).unwrap().len(), 2);
}

/// [`Mempool::candidates_by`] takes each account's nonce from the caller (D-130), once per account in the pool, and
/// gives what [`Mempool::candidates`] gives with the same nonces.
#[test]
fn candidates_by_asks_each_account_its_nonce_once() {
    let (a, b, c) = (user("a"), user("b"), user("c"));
    let mut st = state(&[(&a, 1_000), (&b, 1_000), (&c, 1_000)]);
    let mut p = pool();
    for nonce in 0..3 {
        p.insert(op(&a, nonce, 5), &arc(&st)).unwrap();
    }
    p.insert(op(&b, 1, 7), &arc(&st)).unwrap();
    p.insert(op(&c, 0, 6), &arc(&st)).unwrap();
    set_account(&mut st, &a, 1);
    set_account(&mut st, &b, 1);
    let nonces = BTreeMap::from([(a.account, 1), (b.account, 1), (c.account, 0)]);
    let mut asked = Vec::new();
    let by = p
        .candidates_by(|account| {
            asked.push(*account);
            Ok(nonces[account])
        })
        .unwrap();
    assert_eq!(by, p.candidates(&arc(&st)).unwrap());
    assert_eq!(by.len(), 4);
    assert_eq!(asked, nonces.keys().copied().collect::<Vec<_>>());
    assert_eq!(
        p.candidates_by(|_| Err(AdmitError::State("unreadable".into()))),
        Err(AdmitError::State("unreadable".into()))
    );
}

#[test]
fn candidates_follow_nonce_chains_and_fee_priority() {
    let (a, b, c) = (user("a"), user("b"), user("c"));
    let s = arc(&state(&[(&a, 1_000), (&b, 1_000), (&c, 1_000)]));
    let mut p = pool();
    p.insert(op(&a, 1, 2), &s).unwrap();
    p.insert(op(&b, 1, 7), &s).unwrap();
    p.insert(op(&a, 0, 5), &s).unwrap();
    p.insert(op(&b, 0, 3), &s).unwrap();
    // c's nonce 0 is missing: nonce 1 is not a candidate
    p.insert(op(&c, 1, 50), &s).unwrap();

    let order: Vec<(u8, u64)> = p
        .candidates(&s)
        .unwrap()
        .iter()
        .map(|o| {
            let who = if o.account == a.account { b'a' } else { b'b' };
            (who, o.nonce)
        })
        .collect();
    assert_eq!(order, vec![(b'a', 0), (b'b', 0), (b'b', 1), (b'a', 1)]);

    // Once c's nonce 0 is supplied, both of c's ops become candidates, and whether c0 (2) comes before or after a1 (2)
    // depends on the op_id; both come after b1
    p.insert(op(&c, 0, 2), &s).unwrap();
    let all = p.candidates(&s).unwrap();
    assert_eq!(all.len(), 6);
    let pos = |acc: &[u8; 32], nonce: u64| {
        all.iter()
            .position(|o| o.account == *acc && o.nonce == nonce)
            .unwrap()
    };
    assert!(pos(&c.account, 0) < pos(&c.account, 1));
    assert!(pos(&a.account, 0) < pos(&a.account, 1));
    assert_eq!(pos(&a.account, 0), 0);
}

#[test]
fn remove_committed_drops_included_and_stale_ops() {
    let (a, b) = (user("a"), user("b"));
    let mut st = state(&[(&a, 1_000), (&b, 1_000)]);
    let mut p = pool();
    let a0 = p.insert(op(&a, 0, 5), &arc(&st)).unwrap();
    let a1 = p.insert(op(&a, 1, 5), &arc(&st)).unwrap();
    let a2 = p.insert(op(&a, 2, 5), &arc(&st)).unwrap();
    let b0 = p.insert(op(&b, 0, 5), &arc(&st)).unwrap();

    // A new block included a0, and (e.g. via other nodes) a's nonce has advanced to 2
    set_account(&mut st, &a, 2);
    let gen = p.generation();
    p.remove_committed(&[a0], &arc(&st)).unwrap();
    assert!(!p.contains(&a0) && !p.contains(&a1));
    assert!(p.contains(&a2) && p.contains(&b0));
    assert_eq!(p.len(), 2);
    assert!(p.generation() > gen);
    let next: Vec<u64> = p
        .candidates(&arc(&st))
        .unwrap()
        .iter()
        .map(|o| o.nonce)
        .collect();
    assert_eq!(next.len(), 2);
    assert!(p.get(&a2).is_some());
}

/// Packing candidates are ordered by fee rate (sponsored fee per KiB of cost) rather than absolute amount: cost =
/// max(encoded bytes, bytes equivalent to max_gas), so an op whose max_gas fills the whole block comes later even with
/// a slightly higher sponsored fee; the relay floor and the packing floor are separate: an op that meets the relay
/// floor but not the packing floor (minimum sponsored fee or minimum fee rate) enters the pool and may be forwarded,
/// but is not a candidate, and neither are the same account's later ops.
#[test]
fn candidates_are_ordered_by_fee_rate_and_cut_at_the_packing_floor() {
    let (a, b, c, d) = (user("a"), user("b"), user("c"), user("d"));
    let s = arc(&state(&[
        (&a, 1_000),
        (&b, 1_000),
        (&c, 1_000),
        (&d, 1_000),
    ]));
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            min_token_fee: 3,
            min_relay_token_fee: 1,
            min_fee_per_kib: 0,
            ..Policy::default()
        },
    );
    let token = |amount: u128| FeeMode::Token { amount };
    // a: sponsored fee 5, max_gas 100,000 (cost = bytes); b: sponsored fee 6, max_gas fills the whole block (cost = the
    // whole block's data limit)
    let small = op_with_gas(&a, 0, token(5), DEVNET.chain_id, 100_000);
    let huge = op_with_gas(&b, 0, token(6), DEVNET.chain_id, DEVNET.block_gas_limit);
    let rules = DEVNET.rules(0);
    let small_cost = op_cost(
        &DEVNET,
        &rules,
        small.encode().unwrap().len(),
        small.max_gas,
    );
    let huge_cost = op_cost(&DEVNET, &rules, huge.encode().unwrap().len(), huge.max_gas);
    assert_eq!(
        small_cost as usize,
        small.encode().unwrap().len(),
        "the byte count dominates"
    );
    assert_eq!(
        huge_cost as usize, DEVNET.max_block_data_bytes,
        "the gas equivalent dominates"
    );
    assert!(fee_rate(5, small_cost) > fee_rate(6, huge_cost));
    assert_eq!(fee_rate(6, huge_cost), 0, "6 × 1024 / 81920 rounded down");
    p.insert(small.clone(), &s).unwrap();
    p.insert(huge.clone(), &s).unwrap();
    // c: sponsored fee 2 meets the relay floor 1 but not the packing floor 3: enters the pool but is not a candidate,
    // and neither is its later nonce 1 (sponsored fee 9)
    p.insert(op(&c, 0, 2), &s).unwrap();
    p.insert(op(&c, 1, 9), &s).unwrap();
    assert!(
        matches!(p.insert(op(&d, 0, 0), &s), Err(AdmitError::Policy(_))),
        "below the relay floor"
    );
    assert_eq!(p.len(), 4);
    let order: Vec<[u8; 32]> = p
        .candidates(&s)
        .unwrap()
        .iter()
        .map(|o| o.account)
        .collect();
    assert_eq!(order, vec![a.account, b.account]);

    // Minimum fee rate: b's fee rate 0 does not reach 1
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            min_token_fee: 1,
            min_relay_token_fee: 1,
            min_fee_per_kib: 1,
            ..Policy::default()
        },
    );
    p.insert(small, &s).unwrap();
    p.insert(huge, &s).unwrap();
    let order: Vec<[u8; 32]> = p
        .candidates(&s)
        .unwrap()
        .iter()
        .map(|o| o.account)
        .collect();
    assert_eq!(order, vec![a.account]);

    // When the relay floor is above the packing floor, the packing floor applies
    let relaxed = Policy {
        min_token_fee: 2,
        min_relay_token_fee: 5,
        ..Policy::default()
    };
    assert_eq!(relaxed.relay_floor(), 2);
    assert!(relaxed.packable(2, 0));
    assert!(!relaxed.packable(1, u128::MAX));
}

/// Sybil spam (security audit): accounts funded with dust skip their current nonce and queue the following nonces
/// with the minimum fee. Such ops can never be packed and nothing removes them; they used to fill the pool for good,
/// after which every new op, however packable, was rejected as "entry pool is full". Ops behind a nonce gap must make
/// way for packable ops.
#[test]
fn sybil_gapped_ops_do_not_lock_out_packable_ops() {
    let (h1, h2) = (user("honest 1"), user("honest 2"));
    let sybils: Vec<User> = (0..40).map(|i| user(&format!("sybil {i}"))).collect();
    let mut allocations: Vec<(&User, u128)> = sybils.iter().map(|u| (u, 63)).collect();
    allocations.extend([(&h1, 1_000), (&h2, 1_000)]);
    let s = arc(&state(&allocations));
    let max_ops = 300;
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            max_ops,
            ..Policy::default()
        },
    );
    for u in &sybils {
        for nonce in 1..=20 {
            let _ = p.insert(op(u, nonce, 1), &s);
        }
    }
    assert_eq!(p.len(), max_ops, "the spam fills the pool");
    assert!(
        p.candidates(&s).unwrap().is_empty(),
        "none of it is packable"
    );

    let mut honest = Vec::new();
    for nonce in 0..4 {
        honest.push(p.insert(op(&h1, nonce, 5), &s));
    }
    honest.push(p.insert(op(&h2, 0, 5), &s));
    for result in &honest {
        assert!(
            result.is_ok(),
            "an honest packable op is admitted: {result:?}"
        );
    }
    assert!(p.len() <= max_ops);
    let packable: Vec<[u8; 32]> = p
        .candidates(&s)
        .unwrap()
        .iter()
        .map(|o| o.op_id().unwrap())
        .collect();
    assert_eq!(packable.len(), honest.len());
}

/// The same spam with ops close to `MAX_OP_BYTES`: the pool's total encoded bytes stay within the default byte cap
/// (32 MiB) instead of growing with the op count (about 640 MB at the default count cap of 10 000), and a large
/// honest op still gets in.
#[test]
fn sybil_gapped_ops_stay_within_the_byte_cap() {
    const CAP: usize = 32 * 1024 * 1024;
    let honest = user("honest");
    let sybils: Vec<User> = (0..70).map(|i| user(&format!("sybil {i}"))).collect();
    let mut allocations: Vec<(&User, u128)> = sybils.iter().map(|u| (u, 63)).collect();
    allocations.push((&honest, 1_000));
    let s = arc(&state(&allocations));
    let mut p = Mempool::new(DEVNET, Policy::default());
    let mut spam = 0;
    for u in &sybils {
        for nonce in 1..=8 {
            let o = big_op(u, nonce, 1, 60_000);
            spam += o.encode().unwrap().len();
            let _ = p.insert(o, &s);
        }
    }
    assert!(spam > CAP);
    assert!(
        pool_bytes(&p) <= CAP,
        "the pool holds {} bytes",
        pool_bytes(&p)
    );
    assert!(p.candidates(&s).unwrap().is_empty());

    let large = big_op(&honest, 0, 5, 60_000);
    let id = large.op_id().unwrap();
    assert_eq!(p.insert(large, &s), Ok(id));
    assert!(pool_bytes(&p) <= CAP);
    assert_eq!(p.candidates(&s).unwrap().len(), 1);
}

/// The first op of a consecutive run moves the whole KSA balance away, so after the block that includes it the
/// account's later ops can no longer pay their sponsored fees: they fail at packing on every block. They used to stay
/// in the pool (and packing candidates) forever; the re-check after a chain change drops them.
#[test]
fn ops_left_unpayable_after_a_block_are_dropped() {
    let (a, b) = (user("a"), user("b"));
    let mut st = state(&[(&a, 100), (&b, 100)]);
    let mut p = pool();
    let drain = Action::AssetTransfer {
        asset: KSA_ASSET_ID,
        to: b.account,
        amount: 95,
    };
    let a0 = p.insert(op_doing(&a, 0, 5, drain), &arc(&st)).unwrap();
    let a1 = p.insert(op(&a, 1, 2), &arc(&st)).unwrap();
    let a2 = p.insert(op(&a, 2, 2), &arc(&st)).unwrap();
    let b0 = p.insert(op(&b, 0, 5), &arc(&st)).unwrap();

    // The block includes a0: a's nonce is 1 and its balance 100 − 5 − 95 = 0
    set_account(&mut st, &a, 1);
    set_balance(&mut st, &a, 0);
    set_balance(&mut st, &b, 195);
    p.remove_committed(&[a0], &arc(&st)).unwrap();
    assert!(!p.contains(&a1) && !p.contains(&a2), "a can no longer pay");
    assert!(p.contains(&b0));
    assert_eq!(p.len(), 1);
    assert_eq!(p.candidates(&arc(&st)).unwrap().len(), 1);
}

/// The pool's own invariants, through its public interface: within the op count and byte limits, and the byte count
/// matches the ops it holds.
fn check_limits(p: &Mempool) {
    let ops = p.pending(usize::MAX);
    assert_eq!(ops.len(), p.len());
    assert!(p.len() <= p.policy().max_ops);
    assert!(p.bytes() <= p.policy().max_bytes);
    assert_eq!(p.bytes(), pool_bytes(p));
    for o in &ops {
        assert_eq!(p.get(&o.op_id().unwrap()), Some(o));
    }
}

/// A full pool evicts an account's last op, the lowest standing first: ops behind a nonce gap go before packing
/// candidates whatever their fee, then candidates go by fee rate. A newcomer that ties with the lowest op already in
/// the pool, or stands lower, is rejected and the pool is unchanged.
#[test]
fn a_full_pool_evicts_gapped_ops_first_then_by_fee_rate() {
    let (a, b, c, d, e, g) = (
        user("a"),
        user("b"),
        user("c"),
        user("d"),
        user("e"),
        user("g"),
    );
    let s = arc(&state(&[
        (&a, 1_000),
        (&b, 1_000),
        (&c, 1_000),
        (&d, 1_000),
        (&e, 1_000),
        (&g, 1_000),
    ]));
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            max_ops: 4,
            ..Policy::default()
        },
    );
    p.insert(op(&a, 0, 5), &s).unwrap();
    p.insert(op(&a, 1, 5), &s).unwrap();
    // g's nonce 0 is missing: g2 and g3 are behind a gap, whatever their sponsored fee
    let g2 = p.insert(op(&g, 2, 50), &s).unwrap();
    let g3 = p.insert(op(&g, 3, 50), &s).unwrap();
    assert_eq!(p.len(), 4);

    let b0 = p.insert(op(&b, 0, 1), &s).unwrap();
    assert!(
        !p.contains(&g3) && p.contains(&g2),
        "g's last op goes first"
    );
    let c0 = p.insert(op(&c, 0, 1), &s).unwrap();
    assert!(!p.contains(&g2));
    check_limits(&p);

    // d0 ties with b0 and c0 (same fee and size): the ops already in the pool stay
    let (before, gen) = (p.pending(usize::MAX), p.generation());
    assert!(matches!(
        p.insert(op(&d, 0, 1), &s),
        Err(AdmitError::Policy(_))
    ));
    // A gapped newcomer stands below every candidate
    assert!(matches!(
        p.insert(op(&e, 1, 100), &s),
        Err(AdmitError::Policy(_))
    ));
    assert_eq!((p.pending(usize::MAX), p.generation()), (before, gen));
    // A higher fee rate evicts the candidate that would be packed last: of b0 and c0, the higher op_id
    let d0 = p.insert(op(&d, 0, 2), &s).unwrap();
    let (stays, goes) = if b0 < c0 { (b0, c0) } else { (c0, b0) };
    assert!(p.contains(&d0) && p.contains(&stays) && !p.contains(&goes));
    assert_eq!(p.len(), 4);
    check_limits(&p);
}

/// Ops below the packing floor (kept for other producers) go before this node's packing candidates, the lowest fee
/// rate first, and an account's later ops before its earlier ones.
#[test]
fn unpackable_ops_go_before_packing_candidates() {
    let (u, v, t, pp, q, r, x) = (
        user("u"),
        user("v"),
        user("t"),
        user("p"),
        user("q"),
        user("r"),
        user("x"),
    );
    let s = arc(&state(&[
        (&u, 1_000),
        (&v, 1_000),
        (&t, 1_000),
        (&pp, 1_000),
        (&q, 1_000),
        (&r, 1_000),
        (&x, 1_000),
    ]));
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            min_token_fee: 3,
            min_relay_token_fee: 1,
            max_ops: 3,
            ..Policy::default()
        },
    );
    // u0 is below the packing floor, so u1 is not a candidate either despite its fee
    let u0 = p.insert(op(&u, 0, 1), &s).unwrap();
    let u1 = p.insert(op(&u, 1, 9), &s).unwrap();
    let p0 = p.insert(op(&pp, 0, 3), &s).unwrap();
    let q0 = p.insert(op(&q, 0, 3), &s).unwrap();
    assert!(!p.contains(&u1) && p.contains(&u0));
    let r0 = p.insert(op(&r, 0, 3), &s).unwrap();
    assert!(!p.contains(&u0));
    assert!(p.contains(&p0) && p.contains(&q0) && p.contains(&r0));
    // An op below the packing floor cannot displace a candidate
    assert!(matches!(
        p.insert(op(&t, 0, 2), &s),
        Err(AdmitError::Policy(_))
    ));

    // Among ops below the packing floor, the lowest fee rate goes first
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            min_token_fee: 3,
            min_relay_token_fee: 1,
            max_ops: 3,
            ..Policy::default()
        },
    );
    p.insert(op(&pp, 0, 3), &s).unwrap();
    let t0 = p.insert(op(&t, 0, 2), &s).unwrap();
    let v0 = p.insert(op(&v, 0, 1), &s).unwrap();
    p.insert(op(&q, 0, 3), &s).unwrap();
    assert!(!p.contains(&v0) && p.contains(&t0));
    p.insert(op(&x, 0, 3), &s).unwrap();
    assert!(!p.contains(&t0));
    assert_eq!(p.candidates(&s).unwrap().len(), 3);
}

/// An op queued after one that can move the account's KSA away (a KSA transfer or KSA attached to a call) may be left
/// unable to pay its sponsored fee: such ops go before ops whose fees are covered, whatever their fee rate.
#[test]
fn ops_that_may_be_left_unfunded_go_before_funded_ones() {
    let (x, y, z, w, k) = (user("x"), user("y"), user("z"), user("w"), user("k"));
    let s = arc(&state(&[
        (&x, 100),
        (&y, 100),
        (&z, 1_000),
        (&w, 1_000),
        (&k, 1_000),
    ]));
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            max_ops: 5,
            ..Policy::default()
        },
    );
    let transfer = Action::AssetTransfer {
        asset: KSA_ASSET_ID,
        to: [9; 32],
        amount: 95,
    };
    let call = Action::Call {
        contract: [7; 32],
        method: "deposit".into(),
        args: Vec::new(),
        funds: vec![Funds {
            asset: KSA_ASSET_ID,
            amount: 95,
        }],
    };
    // Admission only counts sponsored fees (1 + 10 ≤ 100); after the transfer or deposit only 4 KSA are left
    let x0 = p.insert(op_doing(&x, 0, 1, transfer), &s).unwrap();
    let x1 = p.insert(op(&x, 1, 10), &s).unwrap();
    let y0 = p.insert(op_doing(&y, 0, 1, call), &s).unwrap();
    let y1 = p.insert(op(&y, 1, 10), &s).unwrap();
    let z0 = p.insert(op(&z, 0, 2), &s).unwrap();
    assert_eq!(p.len(), 5);
    assert_eq!(
        p.candidates(&s).unwrap().len(),
        5,
        "all of them are candidates"
    );

    p.insert(op(&w, 0, 2), &s).unwrap();
    p.insert(op(&k, 0, 2), &s).unwrap();
    assert!(!p.contains(&x1) && !p.contains(&y1));
    assert!(p.contains(&x0) && p.contains(&y0) && p.contains(&z0));
    check_limits(&p);
}

/// The byte limit: the pool counts encoded bytes, evicts to stay within the limit (also when a replacement grows an
/// op), and a newcomer that would have to evict ops standing above it is rejected with the pool unchanged.
#[test]
fn the_byte_limit_evicts_and_rejects_like_the_op_limit() {
    let (g, h, k, m, n) = (user("g"), user("h"), user("k"), user("m"), user("n"));
    let s = arc(&state(&[
        (&g, 1_000),
        (&h, 1_000),
        (&k, 1_000),
        (&m, 1_000),
        (&n, 1_000),
    ]));
    let big = |u: &User, nonce: u64, fee: u128| big_op(u, nonce, fee, 20_000);
    let big_len = big(&h, 0, 100).encode().unwrap().len();
    let small_len = op(&k, 0, 5).encode().unwrap().len();
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            max_bytes: 3 * big_len + small_len + 50,
            ..Policy::default()
        },
    );
    // g's nonce 0 is missing
    let g1 = p.insert(big(&g, 1, 50), &s).unwrap();
    let g2 = p.insert(big(&g, 2, 50), &s).unwrap();
    let h0 = p.insert(big(&h, 0, 100), &s).unwrap();
    let k0 = p.insert(op(&k, 0, 5), &s).unwrap();
    assert_eq!(p.bytes(), 3 * big_len + small_len);
    check_limits(&p);

    // Replacing h0 with a larger op takes the pool over the limit: g's last op goes
    let h0_larger = p.insert(big_op(&h, 0, 110, 20_100), &s).unwrap();
    assert!(!p.contains(&h0) && p.contains(&h0_larger));
    assert!(!p.contains(&g2) && p.contains(&g1));
    check_limits(&p);
    let n0 = p.insert(big(&n, 0, 100), &s).unwrap();
    assert!(!p.contains(&g1));
    assert!(p.contains(&h0_larger) && p.contains(&k0) && p.contains(&n0));
    check_limits(&p);

    // m0's fee rate is below that of every op it would have to evict
    let before = p.pending(usize::MAX);
    let (gen, bytes) = (p.generation(), p.bytes());
    assert!(matches!(
        p.insert(big(&m, 0, 50), &s),
        Err(AdmitError::Policy(_))
    ));
    assert_eq!(p.pending(usize::MAX), before);
    assert_eq!((p.generation(), p.bytes()), (gen, bytes));
    check_limits(&p);
    assert_eq!(p.candidates(&s).unwrap().len(), 3);
}

/// Each account keeps at most `max_gapped_per_account` ops behind a nonce gap: a newcomer beyond the limit is rejected
/// when it would be the account's last op, and otherwise takes the place of the account's last op (lower nonces are
/// worth more: they are closer to being packed). Ops that arrive out of order within the limit all get in.
#[test]
fn ops_behind_a_nonce_gap_are_limited_per_account() {
    let a = user("a");
    let s = arc(&state(&[(&a, 1_000)]));
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            max_gapped_per_account: 2,
            ..Policy::default()
        },
    );
    let policy = |r: Result<[u8; 32], AdmitError>| matches!(r, Err(AdmitError::Policy(_)));
    let a3 = p.insert(op(&a, 3, 1), &s).unwrap();
    let a5 = p.insert(op(&a, 5, 1), &s).unwrap();
    assert!(policy(p.insert(op(&a, 6, 1), &s)));
    let a1 = p.insert(op(&a, 1, 1), &s).unwrap();
    assert!(!p.contains(&a5) && p.contains(&a3) && p.contains(&a1));
    assert!(p.candidates(&s).unwrap().is_empty());

    // a0 and a2 fill the gaps: a0..a3 are consecutive and leave room behind a new gap
    p.insert(op(&a, 0, 1), &s).unwrap();
    p.insert(op(&a, 2, 1), &s).unwrap();
    p.insert(op(&a, 5, 1), &s).unwrap();
    p.insert(op(&a, 6, 1), &s).unwrap();
    assert!(policy(p.insert(op(&a, 7, 1), &s)));
    assert_eq!(p.len(), 6);
    let nonces: Vec<u64> = p.candidates(&s).unwrap().iter().map(|o| o.nonce).collect();
    assert_eq!(nonces, vec![0, 1, 2, 3]);
    check_limits(&p);
}

/// The re-check after a chain change also drops ops the current rules no longer accept (VmOp version, `max_gas`
/// above `BLOCK_GAS_LIMIT`): they could never be packed again.
#[test]
fn the_recheck_drops_ops_the_rules_no_longer_accept() {
    let (a, b) = (user("a"), user("b"));
    let s = arc(&state(&[(&a, 1_000), (&b, 1_000)]));
    let mut p = pool();
    let token = |amount: u128| FeeMode::Token { amount };
    let a0 = p.insert(op(&a, 0, 5), &s).unwrap();
    let b0 = p
        .insert(op_with_gas(&b, 0, token(5), DEVNET.chain_id, 5_000_000), &s)
        .unwrap();
    let rules = DEVNET.rules(0);
    p.set_rules(Rules {
        block_gas_limit: 1_000_000,
        ..rules
    });
    let gen = p.generation();
    p.remove_committed(&[], &s).unwrap();
    assert!(p.contains(&a0) && !p.contains(&b0));
    assert!(p.generation() > gen);

    p.set_rules(Rules {
        min_vmop_version: VMOP_VERSION + 1,
        max_vmop_version: VMOP_VERSION + 1,
        ..rules
    });
    p.remove_committed(&[], &s).unwrap();
    assert!(p.is_empty());
    assert_eq!(p.bytes(), 0);
}

/// After a reorg the ops of the disconnected blocks return to the pool (in execution order) while the account's later
/// ops are still queued: the returned ops fill the gap and nothing queued after them is lost, however many there
/// are. When the gap stays open, the re-check keeps only `max_gapped_per_account` ops behind it (the lowest nonces).
#[test]
fn ops_returned_by_a_reorg_keep_the_ops_queued_after_them() {
    let a = user("a");
    let mut st = state(&[(&a, 1_000)]);
    set_account(&mut st, &a, 5);
    let mut p = Mempool::new(DEVNET, Policy::default());
    for nonce in 5..20 {
        p.insert(op(&a, nonce, 1), &arc(&st)).unwrap();
    }
    // The blocks with a2..a4 are disconnected: a's nonce is 2 again and the ops come back
    set_account(&mut st, &a, 2);
    for nonce in 2..5 {
        p.insert(op(&a, nonce, 1), &arc(&st)).unwrap();
    }
    p.remove_committed(&[], &arc(&st)).unwrap();
    assert_eq!(p.len(), 18);
    assert_eq!(p.candidates(&arc(&st)).unwrap().len(), 18);

    // a1 did not come back: everything is behind a gap
    set_account(&mut st, &a, 1);
    p.remove_committed(&[], &arc(&st)).unwrap();
    let kept: Vec<u64> = p.pending(usize::MAX).iter().map(|o| o.nonce).collect();
    assert_eq!(kept, (2..10).collect::<Vec<u64>>());
    check_limits(&p);
}

/// The nonces the path C ops waiting for forced inclusion take from the account nonce on are not gaps (D-104): a
/// retreat after a BTC reorg returns the path B ops above such an op to the pool, more of them than
/// `max_gapped_per_account` stay, the ops signed above them get in, and in a full pool they stand as the ops they are,
/// not as ops behind a gap. The packing candidates still start at the account nonce: the path B ops follow the path C
/// op in the next block. When the path C op leaves its nonce open (it failed out of turn, or its native transaction
/// left the BTC best chain), the next re-check keeps only `max_gapped_per_account` of them; a path C op above a path B
/// op still in the pool is a gap from the start, as a block runs it first.
#[test]
fn nonces_of_path_c_ops_waiting_for_inclusion_are_not_gaps() {
    let (a, b) = (user("a"), user("b"));
    let mut st = state(&[(&a, 1_000), (&b, 1_000)]);
    set_account(&mut st, &a, 3);
    let limit = Policy {
        max_gapped_per_account: 2,
        ..Policy::default()
    };
    let policy = |r: Result<[u8; 32], AdmitError>| matches!(r, Err(AdmitError::Policy(_)));
    let waiting = |nonce: u64| BTreeMap::from([(a.account, vec![nonce])]);
    let nonces =
        |p: &Mempool| -> Vec<u64> { p.pending(usize::MAX).iter().map(|o| o.nonce).collect() };

    // Without the path C op of nonce 3: two of the returned ops stay behind the gap
    let mut p = Mempool::new(DEVNET, limit.clone());
    for nonce in 4..7 {
        let _ = p.insert(op(&a, nonce, 1), &arc(&st));
    }
    assert_eq!(nonces(&p), vec![4, 5]);

    // With it: all of them, and the op signed above them
    let mut p = Mempool::new(DEVNET, limit.clone());
    p.set_forced(waiting(3));
    for nonce in 4..10 {
        p.insert(op(&a, nonce, 1), &arc(&st)).unwrap();
    }
    p.remove_committed(&[], &arc(&st)).unwrap();
    p.insert(op(&a, 10, 1), &arc(&st)).unwrap();
    assert_eq!(nonces(&p), (4..11).collect::<Vec<u64>>());
    assert!(p.candidates(&arc(&st)).unwrap().is_empty());
    check_limits(&p);

    // The block forcing the path C op takes nonce 3: the path B ops are the next candidates
    set_account(&mut st, &a, 4);
    p.set_forced(BTreeMap::new());
    p.remove_committed(&[], &arc(&st)).unwrap();
    let candidates: Vec<u64> = p
        .candidates(&arc(&st))
        .unwrap()
        .iter()
        .map(|o| o.nonce)
        .collect();
    assert_eq!(candidates, (4..11).collect::<Vec<u64>>());

    // Had it failed out of turn, nonce 3 would be open again
    set_account(&mut st, &a, 3);
    p.remove_committed(&[], &arc(&st)).unwrap();
    assert_eq!(nonces(&p), vec![4, 5]);
    check_limits(&p);

    // A path C op of nonce 4 above the path B op of nonce 3 in the pool: the block forcing it runs it first, out of
    // turn, so nonce 4 is a gap
    let mut p = Mempool::new(DEVNET, limit.clone());
    p.set_forced(waiting(4));
    for nonce in [3, 5, 6] {
        p.insert(op(&a, nonce, 1), &arc(&st)).unwrap();
    }
    assert!(policy(p.insert(op(&a, 7, 1), &arc(&st))));
    assert_eq!(nonces(&p), vec![3, 5, 6]);
    check_limits(&p);

    // A full pool: a's last op stands as a packing candidate, above b's newcomer paying less, until the node no
    // longer expects the path C op
    let mut p = Mempool::new(
        DEVNET,
        Policy {
            max_ops: 2,
            ..limit
        },
    );
    p.set_forced(waiting(3));
    let a4 = p.insert(op(&a, 4, 5), &arc(&st)).unwrap();
    let a5 = p.insert(op(&a, 5, 5), &arc(&st)).unwrap();
    assert!(policy(p.insert(op(&b, 0, 2), &arc(&st))));
    p.set_forced(BTreeMap::new());
    let b0 = p.insert(op(&b, 0, 2), &arc(&st)).unwrap();
    assert!(p.contains(&a4) && !p.contains(&a5) && p.contains(&b0));
    check_limits(&p);
}

/// The path C ops waiting for forced inclusion take an account's nonces in BTC order, the order a block runs them in
/// (D-104): going through them from the account nonce, one whose nonce is not the next one fails out of turn and takes
/// none, even when an op after it fills the nonce below.
#[test]
fn nonces_of_path_c_ops_count_in_btc_order() {
    let a = user("a");
    let mut st = state(&[(&a, 1_000)]);
    set_account(&mut st, &a, 3);
    let limit = Policy {
        max_gapped_per_account: 2,
        ..Policy::default()
    };
    // The path B ops of nonces 5 to 9 the pool keeps with the path C ops of `forced` waiting
    let kept = |forced: Vec<u64>| -> Vec<u64> {
        let mut p = Mempool::new(DEVNET, limit.clone());
        p.set_forced(BTreeMap::from([(a.account, forced)]));
        for nonce in 5..10 {
            let _ = p.insert(op(&a, nonce, 1), &arc(&st));
        }
        p.remove_committed(&[], &arc(&st)).unwrap();
        check_limits(&p);
        p.pending(usize::MAX).iter().map(|o| o.nonce).collect()
    };
    // 3 then 4: both taken in turn, so the ops from 5 on are a run
    assert_eq!(kept(vec![3, 4]), (5..10).collect::<Vec<u64>>());
    // 4 then 3: 4 fails out of turn, so 4 is a gap
    assert_eq!(kept(vec![4, 3]), vec![5, 6]);
    // a nonce already taken, and the same nonce twice, take nothing more
    assert_eq!(kept(vec![2, 3, 3]), vec![5, 6]);
    assert_eq!(kept(vec![3, 2, 3, 4]), (5..10).collect::<Vec<u64>>());
}

/// A deterministic random workload (inserts with nonces around and beyond the account nonce, replacements, ops of
/// various sizes and fees, KSA transfers, dust accounts, blocks that include candidates followed by the re-check)
/// against small limits: after every step the pool stays within its limits and its byte count and indexes agree with
/// its contents; after every re-check no op has a used nonce, no account has more ops behind a gap than allowed, and
/// every account's balance covers the sponsored fees of its queued ops.
#[test]
fn random_workload_keeps_the_pool_consistent() {
    let users: Vec<User> = (0..12).map(|i| user(&format!("user {i}"))).collect();
    let mut st = state(
        &users
            .iter()
            .enumerate()
            .map(|(i, u)| (u, if i % 3 == 0 { 8 } else { 400 }))
            .collect::<Vec<_>>(),
    );
    let policy = Policy {
        min_token_fee: 2,
        min_relay_token_fee: 1,
        max_ops: 40,
        max_bytes: 40 * 300,
        max_ops_per_account: 16,
        max_gapped_per_account: 3,
        ..Policy::default()
    };
    let mut p = Mempool::new(DEVNET, policy.clone());
    let mut nonces = vec![0u64; users.len()];
    let mut balances: Vec<u128> = users
        .iter()
        .enumerate()
        .map(|(i, _)| if i % 3 == 0 { 8 } else { 400 })
        .collect();
    let mut seed: u64 = 0x5eed;
    let mut next = |bound: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % bound
    };
    let (mut admitted, mut evicting, mut rejected_full, mut blocks) = (0, 0, 0, 0);
    // Ops admitted without moving the generation: they cannot alter the packing candidates
    let mut quiet = 0;
    for _ in 0..1_500 {
        if next(6) != 0 {
            let i = next(users.len() as u64) as usize;
            let u = &users[i];
            let nonce = nonces[i] + next(18);
            let fee = 1 + u128::from(next(6));
            let o = match next(4) {
                0 => big_op(u, nonce, fee, next(900) as usize),
                1 => op_doing(
                    u,
                    nonce,
                    fee,
                    Action::AssetTransfer {
                        asset: KSA_ASSET_ID,
                        to: users[(i + 1) % users.len()].account,
                        amount: u128::from(next(300)),
                    },
                ),
                _ => op(u, nonce, fee),
            };
            let len = p.len();
            let replacing = p
                .pending(usize::MAX)
                .iter()
                .any(|q| q.account == o.account && q.nonce == o.nonce);
            let (gen, candidates) = (p.generation(), p.candidates(&arc(&st)).unwrap());
            match p.insert(o, &arc(&st)) {
                Ok(_) => {
                    admitted += 1;
                    // An op that took no one's place but left the count where it was made room
                    if !replacing && p.len() <= len {
                        evicting += 1;
                    }
                    if p.generation() == gen {
                        quiet += 1;
                    }
                }
                Err(AdmitError::Policy(m)) if m.contains("full") => rejected_full += 1,
                Err(_) => assert_eq!(p.generation(), gen),
            }
            // A change that can alter the packing candidates moves the generation (D-130)
            if p.generation() == gen {
                assert_eq!(p.candidates(&arc(&st)).unwrap(), candidates);
            }
        } else {
            // A block: include candidates in order, skipping an account's later ops after a failure
            let mut included = Vec::new();
            let mut blocked = std::collections::BTreeSet::new();
            for o in p.candidates(&arc(&st)).unwrap() {
                let i = users.iter().position(|u| u.account == o.account).unwrap();
                let FeeMode::Token { amount: fee } = o.fee else {
                    unreachable!()
                };
                if blocked.contains(&i) || o.nonce != nonces[i] || balances[i] < fee {
                    blocked.insert(i);
                    continue;
                }
                balances[i] -= fee;
                if let Action::AssetTransfer { to, amount, .. } = &o.action {
                    if *amount <= balances[i] {
                        balances[i] -= amount;
                        if let Some(j) = users.iter().position(|u| u.account == *to) {
                            balances[j] += amount;
                        }
                    }
                }
                nonces[i] += 1;
                included.push(o.op_id().unwrap());
            }
            for (i, u) in users.iter().enumerate() {
                set_account(&mut st, u, nonces[i]);
                set_balance(&mut st, u, balances[i]);
            }
            p.remove_committed(&included, &arc(&st)).unwrap();
            blocks += 1;
            for (i, u) in users.iter().enumerate() {
                let queued: Vec<VmOp> = p
                    .pending(usize::MAX)
                    .into_iter()
                    .filter(|o| o.account == u.account)
                    .collect();
                assert!(queued.iter().all(|o| o.nonce >= nonces[i]));
                let mut run_end = nonces[i];
                while queued.iter().any(|o| o.nonce == run_end) {
                    run_end += 1;
                }
                let gapped = queued.iter().filter(|o| o.nonce > run_end).count();
                assert!(gapped <= policy.max_gapped_per_account);
                let fees: u128 = queued
                    .iter()
                    .map(|o| match o.fee {
                        FeeMode::Token { amount } => amount,
                        FeeMode::BtcNative => unreachable!(),
                    })
                    .sum();
                assert!(fees <= balances[i]);
            }
        }
        check_limits(&p);
    }
    assert!(
        admitted > 200 && evicting > 20 && rejected_full > 20 && blocks > 100 && quiet > 20,
        "{admitted} admitted ({evicting} making room, {quiet} not moving the generation), {rejected_full} rejected as \
         the pool is full, {blocks} blocks"
    );
}
