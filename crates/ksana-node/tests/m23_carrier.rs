//! M23: node behavior of carrier rewards (D-095):
//!
//! - A producer without a funding wallet packs path B as usual, and its blocks carry no settlement transaction; the
//!   body is carried onto BTC by another producer with a settlement transaction, and at unlock the `CARRIER_BPS` of
//!   each of that block's reward slots and the `CARRIER_FEE_BPS` of its path-B sponsored fees go to the carrier (out
//!   of the producer's part; the block share is unchanged); carrying one's own block is not split;
//! - the payload that lands the data first counts; later payloads carrying the same body earn no reward;
//! - the payee of a republish payload counts as the carrier only if the block's data waited more than
//!   `ANCHOR_DEPTH + CARRIER_REPOST_DELAY` BTC blocks;
//! - `locked_fees` of `ksana_getAccount` lists the reward the carrier will receive, and `ksana_getSettlement` gives the
//!   carrier account;
//! - a node that did not take part in carrying executes the same unlock block and gets the same state (the carrier is
//!   determined by the BTC snapshot and the main chain);
//! - the RPC's index of the carriers (`Carriers`, D-132) answers as `carrier_of` for every block, from any height.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;

use bitcoin::{
    absolute, transaction, Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use common::side::{setup_with_share, Side};
use common::{transfer_op, user};
use ksana_btc::mock::MockBtc;
use ksana_btc::payload::{attached_payload, bodies_in, entry_data};
use ksana_btc::settlement::SettlementPair;
use ksana_btc::tracker::carrier_of;
use ksana_execution::keys::{self, AssetMeta, LockedFee};
use ksana_node::btc::Carriers;
use ksana_node::producer::FeePolicy;
use ksana_node::rpc::NodeRpc;
use ksana_primitives::{Block, Body, Entry, Payload, RewardSplit, DEVNET, KSA_ASSET_ID};
use ksana_rpc::{Backend, BlockId};

/// The three parts of one reward slot (weight 1) in period 0 under the devnet rules: producer 75%, share 20%, carrier
/// 5% (D-089, D-095).
fn split() -> RewardSplit {
    let split = DEVNET.block_rewards_split_at(0, 1);
    assert_eq!(split.total(), DEVNET.block_reward);
    assert_eq!(split.share * 5, DEVNET.block_reward);
    assert_eq!(split.carrier * 20, DEVNET.block_reward);
    split
}

fn supply(side: &Side) -> u128 {
    side.chain
        .with_state(|state, _| {
            let meta = state.get(&keys::asset(&KSA_ASSET_ID)).unwrap().unwrap();
            borsh::from_slice::<AssetMeta>(&meta).unwrap().total_supply
        })
        .unwrap()
}

/// KSA conservation: supply = all available balances + all sponsored fee records.
fn assert_conserved(side: &Side) {
    let (balances, locked) = side
        .chain
        .with_state(|state, _| {
            let balances: u128 = state
                .scan_prefix(&[keys::PREFIX_BALANCE])
                .unwrap()
                .into_iter()
                .filter(|(k, _)| k.get(1..33) == Some(KSA_ASSET_ID.as_slice()))
                .map(|(_, v)| borsh::from_slice::<u128>(&v).unwrap())
                .sum();
            let locked: u128 = state
                .scan_prefix(&[keys::PREFIX_LOCKED_FEE])
                .unwrap()
                .into_iter()
                .map(|(_, v)| borsh::from_slice::<LockedFee>(&v).unwrap().amount)
                .sum();
            (balances, locked)
        })
        .unwrap();
    assert_eq!(
        supply(side),
        balances + locked,
        "KSA supply = available balances + sponsored fee records"
    );
}

/// The unlock entry in the block (at most one per block): the acceptance position unlocked up to.
fn unlock_of(block: &Block) -> Option<(u64, u32)> {
    let mut unlocks = block.entries.iter().filter_map(|e| match e {
        Entry::Unlock { height, index } => Some((*height, *index)),
        _ => None,
    });
    let first = unlocks.next();
    assert!(
        unlocks.next().is_none(),
        "at most one unlock entry per block"
    );
    first
}

/// Broadcasts the settlement transactions that have reached `SETTLEMENT_DEPTH` and confirms them on the mock chain.
fn settle(mock: &MockBtc, side: &Side) {
    side.chain.process_settlements().unwrap();
    mock.mine(1);
    side.sync();
}

/// Which blocks' bodies the settlement payload carries.
fn carried_bodies(block: &Block) -> Vec<[u8; 32]> {
    let pair = SettlementPair::decode(
        block
            .settlement
            .as_ref()
            .expect("carries a settlement transaction"),
    )
    .unwrap();
    let payload = Payload::decode(&attached_payload(&DEVNET, &pair).unwrap(), &DEVNET).unwrap();
    bodies_in(&payload).iter().map(|(hash, _)| *hash).collect()
}

/// A transaction with a KSN1 envelope in its witness (D-081): it spends fresh funds on the mock chain, and the witness
/// is a placeholder signature, the envelope leaf and a placeholder control block (the mock chain checks neither scripts
/// nor signatures; the tracker only looks at the witness syntax). In the tests it plays a republish payload that a
/// third party sends to BTC.
fn envelope_tx(mock: &MockBtc, payload: &Payload) -> Transaction {
    let input = mock.fund(&ksana_btc::mock::coinbase_script(), &[10_000])[0];
    let key =
        bitcoin::key::Keypair::from_seckey_slice(&bitcoin::key::Secp256k1::new(), &[0x5a; 32])
            .unwrap()
            .x_only_public_key()
            .0;
    let encoded = payload.encode(&DEVNET).unwrap();
    let leaf = ksana_btc::envelope::envelope_script(&key, &DEVNET.magic, &encoded).unwrap();
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: input,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[vec![0u8; 64], leaf.into_bytes(), vec![0xc0; 33]]),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(ksana_btc::settlement::P2A_AMOUNT_SAT),
            script_pubkey: ksana_btc::settlement::p2a_script(),
        }],
    }
}

/// A third party sends the headers and bodies of `blocks` to the mock chain as one republish payload (payee `payee`)
/// and mines one BTC block to confirm it. The mock chain advances two blocks in total: the block funding the third
/// party and the block confirming the republish.
fn repost(mock: &MockBtc, blocks: &[&Block], payee: [u8; 32]) {
    let payload = Payload::Headers {
        headers: blocks.iter().map(|b| b.header.clone()).collect(),
        bodies: blocks
            .iter()
            .enumerate()
            .map(|(i, b)| Body {
                header: u32::try_from(i).unwrap(),
                entries: entry_data(&b.entries),
            })
            .collect(),
        payee,
    };
    mock.submit(envelope_tx(mock, &payload)).unwrap();
    mock.mine(1);
}

/// This instance's RPC backend (no server started, called directly).
fn rpc(side: &Side) -> NodeRpc {
    NodeRpc {
        chain: Arc::clone(&side.chain),
        mempool: Arc::clone(&side.mempool),
        sync: None,
        paused: Arc::new(AtomicBool::new(false)),
        events: side.producer.events.clone(),
        relay: None,
        fees: FeePolicy::new(1),
        calls: AtomicUsize::new(0),
    }
}

/// The account's income at block `hash` not yet paid out, as listed by `ksana_getAccount`.
fn pending(rpc: &NodeRpc, account: [u8; 32], hash: [u8; 32]) -> Option<u128> {
    rpc.account(account)
        .unwrap()
        .locked_fees
        .iter()
        .find(|l| l.block_hash == hash)
        .map(|l| l.amount)
}

fn carrier_account(rpc: &NodeRpc, hash: [u8; 32]) -> Option<[u8; 32]> {
    rpc.settlement(BlockId::Hash(hash)).unwrap().carrier_account
}

/// The carriers `ksana_getAccount` finds through its index (D-132) are those of `carrier_of`, for every block the main
/// chain accepted and for `others`, whatever height the index starts from.
fn assert_carriers_agree(side: &Side, others: &[[u8; 32]]) {
    let main = side.chain.main_chain().unwrap();
    let snapshot = side.chain.btc().snapshot().unwrap();
    let params = side.chain.params();
    let mut hashes: Vec<[u8; 32]> = main
        .iter()
        .flat_map(|(_, entry)| entry.accepted())
        .map(|block| block.hash)
        .collect();
    hashes.extend_from_slice(others);
    let mut carried = 0;
    for from in 0..=main.tip_height() + 1 {
        let mut carriers = Carriers::new(params, &snapshot, &main, from);
        for hash in &hashes {
            let expected = carrier_of(params, &snapshot, &main, None, hash).unwrap();
            assert_eq!(
                carriers.carrier(hash).unwrap(),
                expected,
                "block {hash:?}, index from height {from}"
            );
            carried += usize::from(expected.is_some());
        }
    }
    assert!(carried > 0, "some block has a carrier");
}

/// B, without a funding wallet, packs one path-B op, and block 1 carries no settlement transaction; A's block 2
/// carries a settlement transaction that carries block 1's body. At unlock, block 1's reward slot is split into
/// producer 75%, share 20%, carrier 5%, and the sponsored fee is halved (rounded down, remainder to the producer);
/// block 2 is carried by A itself and is not split; a later second republish carrying block 1's body does not change
/// the carrier. Before the unlock, the RPC already lists the amounts each party will receive and the carrier.
#[test]
fn a_body_carried_by_another_producers_settlement_pays_the_carrier() {
    let (alice, bob, holder, other) = (
        user("m23/carry/alice"),
        user("m23/carry/bob"),
        user("m23/carry/holder"),
        user("m23/carry/other"),
    );
    let (mock, genesis) = setup_with_share(&[(&alice, 1_000)], Some(holder.account));
    let a = Side::new(&mock, &genesis, "m23carry-a");
    let b = Side::unfunded(&mock, &genesis, "m23carry-b");
    let (pa, pb) = (a.producer.producer, b.producer.producer);
    let split = split();
    let fee1 = DEVNET.carrier_fee_at(0, 5);
    assert_eq!(fee1, 2);

    // Block 1: B packs alice's transfer (sponsored fee 5), no settlement transaction
    b.submit(transfer_op(&alice, 0, 5, bob.account, 10));
    let b1 = b.mine();
    assert_eq!(b1.header.producer, pb);
    assert!(
        b1.entries
            .iter()
            .any(|e| matches!(e, Entry::Sponsored { .. })),
        "path B is packed even without a funding wallet (D-095)"
    );
    assert!(b1.settlement.is_none());
    a.accept(&b1).unwrap();
    assert_eq!(a.ksa(&bob), 10);

    // Block 2: A includes its own op (sponsored fee 3), and the settlement payload carries block 1's body
    a.submit(transfer_op(&alice, 1, 3, bob.account, 1));
    let a2 = a.mine();
    assert_eq!(a2.header.producer, pa);
    assert!(
        carried_bodies(&a2).contains(&b1.header.hash()),
        "the payload carries block 1's body"
    );
    b.accept(&a2).unwrap();
    let mut rest = Vec::new();
    for _ in 0..DEVNET.settlement_depth {
        let empty = a.mine();
        assert!(
            empty.settlement.is_none(),
            "block 1's body is already carried by the in-flight settlement"
        );
        rest.push(empty);
    }
    assert_eq!(a.balance(&pa), 0);
    settle(&mock, &a);

    // After the settlement lands on BTC and before the unlock: the RPC lists the amounts each party will receive and
    // the carrier
    let rpc = rpc(&a);
    let (h1, h2) = (b1.header.hash(), a2.header.hash());
    assert_eq!(carrier_account(&rpc, h1), Some(pa));
    assert_eq!(carrier_account(&rpc, h2), Some(pa), "carries its own block");
    assert_carriers_agree(&a, &[[0x99; 32]]);
    assert_eq!(pending(&rpc, pb, h1), Some(split.producer + 5 - fee1));
    assert_eq!(
        pending(&rpc, pa, h1),
        Some(split.carrier + fee1),
        "the carrier's income at block 1"
    );
    assert_eq!(pending(&rpc, pb, h2), None);
    assert_eq!(
        pending(&rpc, pa, h2),
        Some(split.producer + split.carrier + 3),
        "carried by itself: not split"
    );
    assert_eq!(pending(&rpc, holder.account, h1), Some(split.share));
    assert_eq!(pending(&rpc, holder.account, h2), Some(split.share));
    assert_eq!(pending(&rpc, other.account, h1), None);

    // A second republish carrying block 1's body (payee other): the first to land counts, the carrier is unchanged
    repost(&mock, &[&b1], other.account);
    a.sync();
    assert_eq!(carrier_account(&rpc, h1), Some(pa));
    assert_carriers_agree(&a, &[[0x99; 32]]);

    // Unlock up to block 2: block 1 split three ways and the sponsored fee halved; block 2 carried by A itself, not
    // split
    let unlocker = a.mine();
    assert_eq!(unlock_of(&unlocker), Some((2, 0)));
    assert_eq!(
        a.balance(&pb),
        split.producer + 5 - fee1,
        "B: 75% plus half the sponsored fee"
    );
    assert_eq!(
        a.balance(&pa),
        split.carrier + fee1 + split.producer + split.carrier + 3,
        "A: the carrier's part of block 1 plus 80% of block 2"
    );
    assert_eq!(a.balance(&holder.account), 2 * split.share);
    assert_eq!(a.balance(&other.account), 0);
    assert_eq!(supply(&a), 1_000 + 2 * split.total());
    assert_conserved(&a);
    assert_eq!(pending(&rpc, pa, h1), None);

    // B did not take part in carrying; executing the same unlock block gives the same state
    for block in &rest {
        b.accept(block).unwrap();
    }
    b.accept(&unlocker).unwrap();
    assert_eq!(b.chain.tip().unwrap(), a.chain.tip().unwrap());
    assert_eq!(b.balance(&pa), a.balance(&pa));
    assert_eq!(b.balance(&pb), a.balance(&pb));
}

/// The payee of a republish payload (D-095): a republish that lands the block's data less than
/// `ANCHOR_DEPTH + CARRIER_REPOST_DELAY + 1` BTC blocks after the block was produced does not count as the carrier;
/// only one that waited long enough does; a third party republishing once more does not count. Neither producer has
/// a funding wallet, and the blocks' data lands on BTC only through third-party republishes.
#[test]
fn a_republished_body_pays_the_payee_only_after_the_waiting_window() {
    let (alice, bob, holder, early, late, third) = (
        user("m23/repost/alice"),
        user("m23/repost/bob"),
        user("m23/repost/holder"),
        user("m23/repost/early"),
        user("m23/repost/late"),
        user("m23/repost/third"),
    );
    let (mock, genesis) = setup_with_share(&[(&alice, 1_000)], Some(holder.account));
    let a = Side::unfunded(&mock, &genesis, "m23repost-a");
    let b = Side::unfunded(&mock, &genesis, "m23repost-b");
    let (pa, pb) = (a.producer.producer, b.producer.producer);
    let window = DEVNET.anchor_depth + DEVNET.carrier_repost_delay;
    assert_eq!(window, 2);
    let split = split();

    // Block 1: B packs alice's transfer (sponsored fee 5); its data is republished by early after waiting exactly
    // window BTC blocks: not enough (more than window are required)
    b.submit(transfer_op(&alice, 0, 5, bob.account, 10));
    let b1 = b.mine();
    assert!(b1.settlement.is_none());
    let anchor1 = b1.header.btc_anchor.height;
    assert_eq!(
        anchor1,
        mock.tip_height(),
        "devnet's ANCHOR_DEPTH is 0: anchored at the tip"
    );
    a.accept(&b1).unwrap();
    repost(&mock, &[&b1], early.account);
    assert_eq!(mock.tip_height() - anchor1, window);
    a.sync();
    let rpc = rpc(&a);
    let h1 = b1.header.hash();
    assert_eq!(
        carrier_account(&rpc, h1),
        None,
        "did not wait long enough: no carrier"
    );
    assert_eq!(
        pending(&rpc, pb, h1),
        Some(split.producer + split.carrier + 5)
    );
    assert_eq!(pending(&rpc, early.account, h1), None);
    // Block 2: A's unlock block (unlocking up to block 1): no carrier, both parts go to B
    let a2 = a.mine();
    assert_eq!(unlock_of(&a2), Some((1, 0)));
    assert_eq!(a.balance(&pb), split.producer + split.carrier + 5);
    assert_eq!(a.balance(&holder.account), split.share);
    assert_eq!(a.balance(&early.account), 0);
    b.accept(&a2).unwrap();

    // Block 3: B packs another op (sponsored fee 7); the data of blocks 2 and 3 is republished by late only after
    // waiting window + 1 BTC blocks: both count
    b.submit(transfer_op(&alice, 1, 7, bob.account, 1));
    let b3 = b.mine();
    a.accept(&b3).unwrap();
    let anchor3 = b3.header.btc_anchor.height;
    assert_eq!(anchor3, anchor1 + window);
    assert_eq!(a2.header.btc_anchor.height, anchor3);
    mock.mine(window - 1);
    repost(&mock, &[&a2, &b3], late.account);
    assert_eq!(mock.tip_height() - anchor3, window + 1);
    a.sync();
    let (h2, h3) = (a2.header.hash(), b3.header.hash());
    assert_eq!(carrier_account(&rpc, h2), Some(late.account));
    assert_eq!(carrier_account(&rpc, h3), Some(late.account));
    assert_carriers_agree(&a, &[[0x99; 32]]);
    let fee3 = DEVNET.carrier_fee_at(0, 7);
    assert_eq!(fee3, 3);
    assert_eq!(pending(&rpc, pa, h2), Some(split.producer));
    assert_eq!(pending(&rpc, late.account, h2), Some(split.carrier));
    assert_eq!(pending(&rpc, pb, h3), Some(split.producer + 7 - fee3));
    assert_eq!(pending(&rpc, late.account, h3), Some(split.carrier + fee3));

    // A third party republishes block 3's body once more: the first to land counts
    repost(&mock, &[&b3], third.account);
    a.sync();
    assert_eq!(carrier_account(&rpc, h3), Some(late.account));
    assert_eq!(pending(&rpc, third.account, h3), None);

    // Block 4: A's unlock block (unlocking up to block 3): block 2 (A's, no sponsored fee) and block 3 each split off
    // the carrier's 5%, and block 3's sponsored fee is halved
    let a4 = a.mine();
    assert_eq!(unlock_of(&a4), Some((3, 0)));
    assert_eq!(a.balance(&pa), split.producer);
    assert_eq!(
        a.balance(&pb),
        split.producer + split.carrier + 5 + split.producer + 7 - fee3
    );
    assert_eq!(a.balance(&late.account), 2 * split.carrier + fee3);
    assert_eq!(a.balance(&holder.account), 3 * split.share);
    assert_eq!(a.balance(&early.account), 0);
    assert_eq!(a.balance(&third.account), 0);
    assert_eq!(supply(&a), 1_000 + 3 * split.total());
    assert_conserved(&a);

    // B executes the same unlock block and gets the same state
    b.accept(&a4).unwrap();
    assert_eq!(b.chain.tip().unwrap(), a.chain.tip().unwrap());
    assert_eq!(b.balance(&late.account), a.balance(&late.account));
}
