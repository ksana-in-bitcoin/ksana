//! M9: node behavior of block rewards (D-074, D-079):
//!
//! - No reward is paid when executing a block: a block without entries does not change the state, and its state root
//!   equals the selected parent's (D-079); sponsored fees are recorded at the block's acceptance position when
//!   received;
//! - once the settlement lands on BTC and the block enters the settled closure, the producer writes one cumulative
//!   unlock entry that unlocks in acceptance order up to the last settled position: the block rewards of those blocks
//!   are paid out now (the KSA supply grows accordingly) and the sponsored fees are released; a block with an unlock
//!   entry has a settlement transaction whose payload carries the headers of the preceding empty blocks, so the empty
//!   blocks' rewards are paid out next once it lands on BTC;
//! - at any time KSA supply = available balances + sponsored fee records; `ksana_getAsset` reports the supply (only
//!   rewards already paid out), `ksana_getAccount` lists the rewards and sponsored fees after the unlock cursor that
//!   are not yet paid out;
//! - the weight of empty blocks grows with the streak (12, 24, 48, ..., 120), occupying as many reward slots as DAA
//!   score (D-084, D-090);
//! - when the genesis config gives a block share account, the `BLOCK_SHARE_BPS` (devnet 20%) of every reward slot is
//!   credited to it at unlock and the rest goes to the producer (D-089).

mod common;

use std::time::Duration;

use common::mock_node::{signed, wait, MockNode};
use common::side::{setup, setup_with_share, Side};
use common::{transfer_op, user};
use ksana_execution::keys::{self, AssetMeta, LockedFee, UnlockCursor};
use ksana_primitives::{Block, Entry, FeeMode, DEVNET, KSA_ASSET_ID};
use ksana_rpc::KsanaApiClient;

const REWARD: u128 = DEVNET.block_reward;
/// The block share account's share of each reward slot (devnet `BLOCK_SHARE_BPS` 2 000, D-089).
const SHARE: u128 = REWARD / 5;

/// The total number of reward slots of `count` consecutive empty blocks after a non-empty block (weights 12, 24, 48,
/// ... by streak, D-084, D-090).
fn empty_slots(count: u64) -> u128 {
    (1..=count)
        .map(|streak| u128::from(DEVNET.empty_block_weight(streak)))
        .sum()
}

fn supply(side: &Side) -> u128 {
    side.chain
        .with_state(|state, _| {
            let meta = state.get(&keys::asset(&KSA_ASSET_ID)).unwrap().unwrap();
            borsh::from_slice::<AssetMeta>(&meta).unwrap().total_supply
        })
        .unwrap()
}

/// The unlock cursor (the acceptance position unlocked up to).
fn cursor(side: &Side) -> Option<(u64, u32)> {
    side.chain
        .with_state(|state, _| {
            state
                .get(&keys::unlock_cursor())
                .unwrap()
                .map(|v| borsh::from_slice::<UnlockCursor>(&v).unwrap())
                .map(|c| (c.height, c.index))
        })
        .unwrap()
}

/// All sponsored fee records: acceptance position and amount.
fn fee_records(side: &Side) -> Vec<((u64, u32), u128)> {
    side.chain
        .with_state(|state, _| {
            state
                .scan_prefix(&[keys::PREFIX_LOCKED_FEE])
                .unwrap()
                .into_iter()
                .map(|(k, v)| {
                    (
                        keys::locked_fee_position(&k).unwrap(),
                        borsh::from_slice::<LockedFee>(&v).unwrap().amount,
                    )
                })
                .collect()
        })
        .unwrap()
}

/// KSA conservation: supply = all available balances + all sponsored fee records.
fn assert_conserved(side: &Side) {
    let balances: u128 = side
        .chain
        .with_state(|state, _| {
            state
                .scan_prefix(&[keys::PREFIX_BALANCE])
                .unwrap()
                .into_iter()
                .filter(|(k, _)| k.get(1..33) == Some(KSA_ASSET_ID.as_slice()))
                .map(|(_, v)| borsh::from_slice::<u128>(&v).unwrap())
                .sum()
        })
        .unwrap();
    let locked: u128 = fee_records(side).iter().map(|(_, amount)| amount).sum();
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
fn settle(mock: &ksana_btc::mock::MockBtc, side: &Side) {
    side.chain.process_settlements().unwrap();
    mock.mine(1);
    side.sync();
}

#[test]
fn rewards_are_issued_when_positions_are_unlocked() {
    let (alice, bob) = (user("m9/alice"), user("m9/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "m9a");
    let producer = a.producer.producer;
    let depth = usize::try_from(DEVNET.settlement_depth).unwrap();

    // Block 1 collects a 5 KSA sponsored fee and carries a settlement transaction; then SETTLEMENT_DEPTH empty blocks:
    // they do not change the state (D-079)
    a.submit(transfer_op(&alice, 0, 5, bob.account, 10));
    let b1 = a.mine();
    assert!(b1.settlement.is_some());
    let empty: Vec<Block> = (0..depth).map(|_| a.mine()).collect();
    assert!(empty
        .iter()
        .all(|b| b.entries.is_empty() && b.settlement.is_none()));
    assert!(
        empty
            .iter()
            .all(|b| b.header.state_root == b1.header.state_root),
        "an empty block's state root equals its parent's"
    );
    assert_eq!(supply(&a), 1_000, "rewards are paid out only at unlock");
    assert_eq!(a.balance(&producer), 0);
    assert_eq!(fee_records(&a), vec![((1, 0), 5)]);
    assert_eq!(cursor(&a), None);
    assert_conserved(&a);

    // Block 1's settlement lands on BTC after reaching the depth: the next block unlocks cumulatively up to block 1
    // (the headers of the later empty blocks are not on BTC yet, so the unlock stops at block 1)
    settle(&mock, &a);
    let unlocker = a.mine();
    assert_eq!(unlock_of(&unlocker), Some((1, 0)));
    assert_eq!(a.balance(&producer), REWARD + 5);
    assert_eq!(supply(&a), 1_000 + REWARD);
    assert!(fee_records(&a).is_empty());
    assert!(
        unlocker.settlement.is_some(),
        "a block with an unlock entry has a settlement transaction whose payload carries the preceding empty blocks' headers"
    );
    assert_conserved(&a);

    // The unlock block's settlement lands on BTC after reaching the depth: it and the preceding empty blocks all enter
    // the settled closure, and one entry unlocks up to it
    let later: Vec<Block> = (0..depth).map(|_| a.mine()).collect();
    assert!(later.iter().all(|b| unlock_of(b).is_none()));
    settle(&mock, &a);
    let next = a.mine();
    let unlocker_position = (unlocker.header.height, 0);
    assert_eq!(unlock_of(&next), Some(unlocker_position));
    assert_eq!(cursor(&a), Some(unlocker_position));
    // Block 1, the following empty blocks (streak 1-SETTLEMENT_DEPTH, 12, 24, ... reward slots each, D-084, D-090) and
    // the unlock block
    let slots = 2 + empty_slots(DEVNET.settlement_depth);
    assert_eq!(a.balance(&producer), slots * REWARD + 5);
    assert_eq!(supply(&a), 1_000 + slots * REWARD);
    assert_conserved(&a);
}

/// In-process node: `ksana_getAsset` reports the KSA supply (genesis allocations plus rewards already paid out),
/// `ksana_getAccount` lists the producer's rewards and sponsored fees not yet paid out; after the settlement lands on
/// BTC the rewards unlock into the producer's available balance.
#[test]
fn rpc_reports_issuance_and_locked_rewards() {
    let (alice, bob) = (user("m9/rpc/alice"), user("m9/rpc/bob"));
    let n = MockNode::start_with(&[(&alice, 1_000)], Duration::from_millis(100));
    wait("block 2", || (n.tip_height() >= 2).then_some(()));
    let asset =
        n.rt.block_on(n.client.get_asset(hex::encode(KSA_ASSET_ID)))
            .unwrap();
    assert_eq!(asset.name, "KSA");
    assert_eq!(asset.decimals, 8);
    assert_eq!(asset.issuer, [0; 32]);
    // No unlock yet: rewards not yet paid out
    assert_eq!(asset.total_supply, 1_000);
    let missing =
        n.rt.block_on(n.client.get_asset(hex::encode([7u8; 32])))
            .unwrap_err();
    assert!(
        missing.to_string().contains("asset does not exist"),
        "{missing}"
    );

    // A single producer, no merged blocks, all empty: the block at height h is the h-th empty block of the streak, with
    // weight 12, 24, 48, 96, 120, 120... (D-084, D-090), and each has as many reward slots not yet paid out
    let account = n.account(&n.producer);
    let height = account.at.height;
    assert!(account.locked_fees.len() >= 2);
    assert_eq!(account.locked_fees.len(), usize::try_from(height).unwrap());
    let mut amounts: Vec<u128> = account.locked_fees.iter().map(|l| l.amount).collect();
    amounts.sort_unstable();
    let mut expected: Vec<u128> = (1..=height)
        .map(|h| u128::from(DEVNET.empty_block_weight(h)) * REWARD)
        .collect();
    expected.sort_unstable();
    assert_eq!(amounts, expected);
    assert_eq!(n.ksa(&n.producer), 0);

    // One path-B op makes the block carry a settlement transaction; once it lands on BTC and enters the settled
    // closure, the rewards and the sponsored fee unlock
    let id = n.submit(&signed(
        &alice,
        0,
        FeeMode::Token { amount: 5 },
        bob.account,
        1,
    ));
    let included = n.wait_included(&id);
    let fee_block = included.block.unwrap();
    let locked = n.account(&n.producer).locked_fees;
    assert!(
        locked
            .iter()
            .any(|l| l.block_hash == fee_block.hash && l.amount == REWARD + 5),
        "{locked:?}"
    );
    let released = wait("rewards unlock", || {
        n.mock.mine(1);
        std::thread::sleep(Duration::from_millis(300));
        let balance = n.ksa(&n.producer);
        (balance > 0).then_some(balance)
    });
    assert!(released >= REWARD, "{released}");
    let asset =
        n.rt.block_on(n.client.get_asset(hex::encode(KSA_ASSET_ID)))
            .unwrap();
    assert!(asset.total_supply > 1_000);
    // Upper bound: the block at height h is at most the h-th empty block of the streak
    assert!(asset.total_supply <= 1_000 + empty_slots(asset.at.height) * REWARD);
}

/// D-089: when the genesis config has a block share account, every reward slot paid out at unlock (main-chain blocks,
/// the w slots of empty blocks) is split by `BLOCK_SHARE_BPS` into the producer's and the share account's parts;
/// sponsored fees are not split; KSA is still conserved.
#[test]
fn the_block_share_is_paid_to_the_genesis_account_at_unlock() {
    let (alice, bob, holder) = (
        user("m9/share/alice"),
        user("m9/share/bob"),
        user("m9/share/holder"),
    );
    assert_eq!(DEVNET.block_share_bps, 2_000);
    assert_eq!(SHARE * 5, REWARD);
    let (mock, genesis) = setup_with_share(&[(&alice, 1_000)], Some(holder.account));
    let a = Side::new(&mock, &genesis, "m9share");
    let producer = a.producer.producer;
    let depth = usize::try_from(DEVNET.settlement_depth).unwrap();

    // Block 1 collects a 5 KSA sponsored fee and carries a settlement transaction, then SETTLEMENT_DEPTH empty blocks;
    // once block 1 lands on BTC the unlock reaches it
    a.submit(transfer_op(&alice, 0, 5, bob.account, 10));
    let b1 = a.mine();
    assert!(b1.settlement.is_some());
    for _ in 0..depth {
        a.mine();
    }
    assert_eq!(a.balance(&holder.account), 0);
    settle(&mock, &a);
    let unlocker = a.mine();
    assert_eq!(unlock_of(&unlocker), Some((1, 0)));
    assert_eq!(
        a.balance(&producer),
        REWARD - SHARE + 5,
        "producer 80% plus the sponsored fee"
    );
    assert_eq!(a.balance(&holder.account), SHARE, "share account 20%");
    assert_eq!(supply(&a), 1_000 + REWARD);
    assert_conserved(&a);

    // After the unlock block lands on BTC: block 1, the empty blocks (12, 24, ... slots each) and the unlock block
    // unlock together, and every slot is split 80% / 20%
    for _ in 0..depth {
        a.mine();
    }
    settle(&mock, &a);
    let next = a.mine();
    assert_eq!(unlock_of(&next), Some((unlocker.header.height, 0)));
    let slots = 2 + empty_slots(DEVNET.settlement_depth);
    assert_eq!(a.balance(&producer), slots * (REWARD - SHARE) + 5);
    assert_eq!(a.balance(&holder.account), slots * SHARE);
    assert_eq!(supply(&a), 1_000 + slots * REWARD);
    assert_conserved(&a);
}

/// In-process node (D-089): `ksana_getAccount` lists for the block share account each block's share not yet paid out
/// (20% per block), and for the producer the remaining 80% and the sponsored fees; after unlock both go into the
/// available balance.
#[test]
fn rpc_lists_the_pending_block_share() {
    let (alice, bob, holder) = (
        user("m9/rpc-share/alice"),
        user("m9/rpc-share/bob"),
        user("m9/rpc-share/holder"),
    );
    let n = MockNode::start_full(
        DEVNET,
        &[(&alice, 1_000)],
        Some(holder.account),
        Duration::from_millis(100),
        |mock| mock,
        |funding| funding,
    );
    wait("block 2", || (n.tip_height() >= 2).then_some(()));
    // No unlock yet: query the producer first, then the share account; every block listed by the former is listed by
    // the latter too, with 1/4 of the amount (all are empty blocks without sponsored fees)
    let producer = n.account(&n.producer);
    let pending = n.account(&holder);
    assert!(producer.locked_fees.len() >= 2);
    for l in &producer.locked_fees {
        let share = pending
            .locked_fees
            .iter()
            .find(|s| s.block_hash == l.block_hash)
            .unwrap_or_else(|| {
                panic!(
                    "the share account has no share of block {}",
                    hex::encode(l.block_hash)
                )
            });
        assert_eq!(share.amount * 4, l.amount, "{:?}", l.block_hash);
        assert_eq!((share.amount + l.amount) % REWARD, 0);
    }
    assert_eq!(n.ksa(&holder), 0);

    // One path-B op: the producer's part of that block includes the sponsored fee, the share account's does not
    let id = n.submit(&signed(
        &alice,
        0,
        FeeMode::Token { amount: 5 },
        bob.account,
        1,
    ));
    let included = n.wait_included(&id);
    let fee_block = included.block.unwrap();
    let locked = n.account(&n.producer).locked_fees;
    assert!(
        locked
            .iter()
            .any(|l| l.block_hash == fee_block.hash && l.amount == REWARD - SHARE + 5),
        "{locked:?}"
    );
    let pending = n.account(&holder).locked_fees;
    assert!(
        pending
            .iter()
            .any(|l| l.block_hash == fee_block.hash && l.amount == SHARE),
        "{pending:?}"
    );

    // After landing on BTC and unlocking: producer balance = share account balance × 4 + sponsored fee (when two reads
    // of the producer balance agree, no new unlock happened in between)
    let (released_producer, released_share) = wait("rewards unlock", || {
        n.mock.mine(1);
        std::thread::sleep(Duration::from_millis(300));
        let before = n.ksa(&n.producer);
        let share = n.ksa(&holder);
        let after = n.ksa(&n.producer);
        (share > 0 && before == after).then_some((after, share))
    });
    assert_eq!(released_producer, released_share * 4 + 5);
    assert!(released_share >= SHARE, "{released_share}");
    assert!(
        n.account(&holder)
            .locked_fees
            .iter()
            .all(|l| l.block_hash != fee_block.hash),
        "no longer listed after unlock"
    );
}
