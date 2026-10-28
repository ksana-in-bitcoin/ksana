//! Block rewards (D-074, D-079): no reward is recorded when a block executes; rewards are paid at unlock time by the
//! unlockable segment — the main-chain block and the blue merged blocks are rewarded by their own DAA scores, halving
//! by DAA score (in periods that may double in length, D-105), and the KSA total supply grows when they are paid and is
//! capped; red blocks get no reward. So when neither the main-chain block nor the blocks it merges have any entries, no
//! state is written. An empty block of weight w takes the reward slots of w DAA scores (D-084; w by streak 12, 24, …,
//! 120, D-090). When the genesis config gives a block share account, every reward slot is split by `BLOCK_SHARE_BPS`
//! into a producer part and a share account part (D-089); when the block has a carrier, each slot further gives
//! `CARRIER_BPS` of the producer's part to the carrier, and sponsored fees are split by `CARRIER_FEE_BPS` (D-095).

mod common;

use common::*;
use ksana_execution::keys::{self, AssetMeta, LockedFee};
use ksana_execution::{BlockError, GenesisConfig, MergedBlock, OpStatus};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Entry, Params, RewardSplit, DEVNET};

const GAS: u64 = 100_000;
const REWARD: u128 = DEVNET.block_reward;

fn supply(chain: &Chain) -> u128 {
    chain
        .get::<AssetMeta>(&keys::asset(&KSA))
        .unwrap()
        .total_supply
}

fn merged(seed: &str, producer: &User, entries: Vec<Entry>) -> MergedBlock {
    MergedBlock {
        hash: sha256(seed.as_bytes()),
        producer: producer.account,
        entries,
        fast_path_paused: false,
    }
}

fn blue(daa_score: u64) -> Color {
    Color {
        blue: true,
        daa_score,
        weight: 1,
    }
}

fn red(daa_score: u64) -> Color {
    Color {
        blue: false,
        daa_score,
        weight: 1,
    }
}

/// KSA conservation: total supply = all available balances + all sponsored-fee records (block rewards enter the total
/// supply only when paid).
fn assert_conserved(chain: &Chain) {
    let mut held: u128 = 0;
    for (key, value) in &chain.state.0 {
        match key.first() {
            Some(&keys::PREFIX_BALANCE) if key.get(1..33) == Some(KSA.as_slice()) => {
                held += borsh::from_slice::<u128>(value).unwrap();
            }
            Some(&keys::PREFIX_LOCKED_FEE) => {
                held += borsh::from_slice::<LockedFee>(value).unwrap().amount;
            }
            _ => {}
        }
    }
    assert_eq!(
        supply(chain),
        held,
        "KSA total supply = available balances + sponsored-fee records"
    );
}

/// An empty block (neither the main-chain block nor the blocks it merges have entries) writes no state: the state root
/// equals the selected parent's (D-079). The same with block rewards.
#[test]
fn empty_blocks_write_nothing() {
    let a = user("a");
    let mut chain = Chain::with_params(DEVNET, vec![(a.account, 1_000)]);
    let genesis = chain.state.0.clone();
    for _ in 0..3 {
        let out = chain.block(vec![]).unwrap();
        assert!(out.writes.is_empty(), "{:?}", out.writes);
    }
    let p = user("p");
    let out = chain
        .colored_block(
            vec![
                (merged("m1", &p, vec![]), blue(2)),
                (merged("m2", &p, vec![]), red(3)),
            ],
            vec![],
        )
        .unwrap();
    assert!(out.writes.is_empty());
    assert_eq!(out.merged_receipts, vec![Vec::new(), Vec::new()]);
    assert_eq!(chain.state.0, genesis);
    assert_eq!(supply(&chain), 1_000, "the reward has not been paid yet");
}

/// A main-chain block's reward is paid when its position is unlocked: it cannot be unlocked before the block enters the
/// settled closure; when unlocked afterwards, the reward is credited directly to the producer's balance and the KSA
/// total supply grows.
#[test]
fn a_chain_block_reward_is_issued_when_its_position_is_unlocked() {
    let a = user("a");
    let mut chain = Chain::with_params(DEVNET, vec![(a.account, 1_000)]);
    let producer = chain.producer;
    assert_eq!(supply(&chain), 1_000);

    chain.block(vec![]).unwrap();
    let block1 = chain.parent;
    chain.block(vec![]).unwrap();
    assert_eq!(supply(&chain), 1_000);
    assert_eq!(chain.ksa(&producer), 0);

    let rejected = chain.block(vec![unlock(1, 0)]);
    assert!(
        matches!(&rejected, Err(BlockError::Invalid(m)) if m.contains("settled closure")),
        "{rejected:?}"
    );
    chain.btc.settle(block1);
    chain.block(vec![unlock(1, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), REWARD);
    assert_eq!(supply(&chain), 1_000 + REWARD);
    assert_conserved(&chain);
    // Block 2's reward has not been paid yet
    let block2 = sha256(&2u64.to_le_bytes());
    chain.btc.settle(block2);
    chain.block(vec![unlock(2, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), 2 * REWARD);
    assert_eq!(supply(&chain), 1_000 + 2 * REWARD);
    assert_conserved(&chain);
}

/// Path-B sponsored fees (recorded at the acceptance position when received) and the block reward (paid at unlock
/// time) are unlocked together.
#[test]
fn fees_and_the_reward_are_released_together() {
    let (a, b) = (user("a"), user("b"));
    let mut chain = Chain::with_params(DEVNET, vec![(a.account, 1_000)]);
    let producer = chain.producer;
    chain
        .block(vec![sponsored(&a, 0, 7, GAS, transfer(b.account, 1))])
        .unwrap();
    let block1 = chain.parent;
    assert_eq!(
        chain.locked_fee(&block1),
        Some(LockedFee {
            producer,
            amount: 7
        }),
        "the record holds only the sponsored fee"
    );
    assert_eq!(supply(&chain), 1_000);
    assert_conserved(&chain);
    chain.btc.settle(block1);
    chain.block(vec![unlock(1, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), REWARD + 7);
    assert_eq!(chain.locked_fee(&block1), None);
    assert_eq!(supply(&chain), 1_000 + REWARD);
    assert_conserved(&chain);
}

/// A blue merged block is rewarded by its own DAA score, to its own producer; a red block gets no reward (only the
/// sponsored fees). Unlocking follows acceptance order: merged blocks first, the main-chain block itself last.
#[test]
fn blue_merged_blocks_are_rewarded_and_red_ones_are_not() {
    let (alice, bob) = (user("alice"), user("bob"));
    let (p1, p2, p3) = (user("p1"), user("p2"), user("p3"));
    let params = Params {
        reward_halving_interval: 10,
        ..DEVNET
    };
    let mut chain = Chain::with_params(params, vec![(alice.account, 100)]);
    let producer = chain.producer;
    // DAA score 12: halved once
    let m_blue = merged("blue", &p1, vec![]);
    let m_red = merged(
        "red",
        &p2,
        vec![sponsored(&alice, 0, 3, GAS, transfer(bob.account, 1))],
    );
    let m_red_empty = merged("red-empty", &p3, vec![]);
    let out = chain
        .colored_block(
            vec![
                (m_blue.clone(), blue(12)),
                (m_red.clone(), red(13)),
                (m_red_empty.clone(), red(14)),
            ],
            vec![],
        )
        .unwrap();
    assert_eq!(out.merged_receipts[1][0].status, OpStatus::Success);
    assert_eq!(chain.position(&m_blue.hash), (1, 0));
    assert_eq!(chain.position(&m_red.hash), (1, 1));
    assert_eq!(chain.position(&chain.parent), (1, 3));
    assert_eq!(chain.locked_fee(&m_blue.hash), None);
    assert_eq!(
        chain.locked_fee(&m_red.hash),
        Some(LockedFee {
            producer: p2.account,
            amount: 3
        })
    );
    assert_eq!(supply(&chain), 100);

    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(1, 3)]).unwrap();
    assert_eq!(chain.ksa(&p1.account), REWARD / 2);
    assert_eq!(
        chain.ksa(&p2.account),
        3,
        "a red block only gets the sponsored fees"
    );
    assert_eq!(chain.ksa(&p3.account), 0);
    // The main-chain block itself (DAA score 1)
    assert_eq!(chain.ksa(&producer), REWARD);
    assert_eq!(supply(&chain), 100 + REWARD + REWARD / 2);
    assert_conserved(&chain);
}

/// Rewards halve by DAA score: with interval 2, the main-chain blocks of DAA scores 1-5 get R, R/2, R/2, R/4, R/4 in
/// turn; 128 or more halvings give 0.
#[test]
fn rewards_halve_by_daa_score() {
    let a = user("a");
    let params = Params {
        block_reward: 1_000,
        reward_halving_interval: 2,
        ..DEVNET
    };
    let mut chain = Chain::with_params(params, vec![(a.account, 1)]);
    let producer = chain.producer;
    for _ in 0..5 {
        chain.block(vec![]).unwrap();
        chain.btc.settle(chain.parent);
    }
    let mut minted = Vec::new();
    for height in 1..=5 {
        let before = supply(&chain);
        chain.block(vec![unlock(height, 0)]).unwrap();
        minted.push(supply(&chain) - before);
    }
    assert_eq!(minted, vec![1_000, 500, 500, 250, 250]);
    assert_eq!(chain.ksa(&producer), 2_500);

    // The merged block has DAA score 256: no reward; the main-chain block itself (height 11, DAA score 11) gets R/32
    let p = user("p");
    let exhausted = merged("exhausted", &p, vec![]);
    chain
        .colored_block(vec![(exhausted.clone(), blue(256))], vec![])
        .unwrap();
    chain.btc.settle(chain.parent);
    let target = chain.position(&chain.parent);
    assert_eq!(target, (11, 1));
    let before = supply(&chain);
    chain.block(vec![unlock(target.0, target.1)]).unwrap();
    // The unlocking blocks at heights 6-10 get R/8, R/8, R/16, R/16, R/32 each, the main-chain block at height 11 R/32
    assert_eq!(supply(&chain) - before, 125 + 125 + 62 + 62 + 31 + 31);
    assert_eq!(chain.ksa(&p.account), 0);
    assert_conserved(&chain);
}

/// D-105: the halving periods double in length `REWARD_INTERVAL_DOUBLINGS` times and then keep the last length: with
/// interval 2 and one doubling the periods are 2, 4, 4, … DAA scores long, so the main-chain blocks of DAA scores 1-7
/// get R, R/2, R/2, R/2, R/2, R/4, R/4 in turn; an empty block whose reward slots cross two period boundaries is paid
/// slot by slot.
#[test]
fn halving_periods_double_in_length() {
    let a = user("a");
    let params = Params {
        block_reward: 1_000,
        reward_halving_interval: 2,
        reward_interval_doublings: 1,
        ..DEVNET
    };
    let mut chain = Chain::with_params(params.clone(), vec![(a.account, 1)]);
    let producer = chain.producer;
    for _ in 0..7 {
        chain.block(vec![]).unwrap();
        chain.btc.settle(chain.parent);
    }
    let mut minted = Vec::new();
    for height in 1..=7 {
        let before = supply(&chain);
        chain.block(vec![unlock(height, 0)]).unwrap();
        minted.push(supply(&chain) - before);
    }
    assert_eq!(minted, vec![1_000, 500, 500, 500, 500, 250, 250]);
    assert_eq!(chain.ksa(&producer), 3_500);
    assert_conserved(&chain);

    // A blue empty block of weight 12 and DAA score 12 takes slots 1-12: 1 000 for slot 1, 500 each for 2-5, 250 each
    // for 6-9, 125 each for 10-12; the main-chain block itself (DAA score 1) gets 1 000
    let p = user("p");
    let mut chain = Chain::with_params(params.clone(), vec![]);
    let producer = chain.producer;
    let empty = merged("empty-across-periods", &p, vec![]);
    chain
        .colored_block(
            vec![(
                empty,
                Color {
                    blue: true,
                    daa_score: 12,
                    weight: 12,
                },
            )],
            vec![],
        )
        .unwrap();
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(1, 1)]).unwrap();
    assert_eq!(
        params.block_rewards_at(12, 12),
        1_000 + 4 * 500 + 4 * 250 + 3 * 125
    );
    assert_eq!(chain.ksa(&p.account), 4_375);
    assert_eq!(chain.ksa(&producer), 1_000);
    assert_eq!(supply(&chain), 5_375);
    assert_conserved(&chain);
}

/// One unlock entry pays the rewards of main-chain and merged blocks together, in acceptance order.
#[test]
fn a_cumulative_unlock_releases_chain_and_merged_rewards_in_order() {
    let p1 = user("p1");
    let mut chain = Chain::with_params(DEVNET, vec![(p1.account, 1)]);
    let producer = chain.producer;
    chain.block(vec![]).unwrap();
    // Block 2 merges m: acceptance order (1,0) block 1, (2,0) m, (2,1) block 2
    let m = merged("m", &p1, vec![]);
    chain
        .colored_block(vec![(m.clone(), blue(1))], vec![])
        .unwrap();
    chain.block(vec![]).unwrap();
    assert_eq!(chain.position(&m.hash), (2, 0));

    // m has entered the settled closure: unlocking up to m also unlocks block 1 before it, block 2 is untouched
    chain.btc.settle(m.hash);
    chain.block(vec![unlock(2, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), REWARD);
    assert_eq!(chain.ksa(&p1.account), 1 + REWARD);
    assert_eq!(chain.cursor(), Some((2, 0)));
    assert_eq!(supply(&chain), 1 + 2 * REWARD);
    assert_conserved(&chain);
}

/// The total genesis allocation must not exceed `KSA_MAX_SUPPLY − upper bound of total rewards`.
#[test]
fn genesis_allocations_are_capped() {
    let (a, b) = (user("a"), user("b"));
    let cap = DEVNET.max_genesis_supply();
    assert!(cap > 0);
    GenesisConfig::devnet(vec![(a.account, cap)])
        .check_supply(&DEVNET)
        .unwrap();
    let err = GenesisConfig::devnet(vec![(a.account, cap), (b.account, 1)])
        .check_supply(&DEVNET)
        .unwrap_err();
    assert!(
        matches!(&err, BlockError::Invalid(m) if m.contains("exceeds the limit")),
        "{err:?}"
    );
}

/// The total supply is capped at `KSA_MAX_SUPPLY` (D-074): parallel blue blocks may have the same DAA score, so the sum
/// of rewards paid per block can slightly exceed the bound obtained by summing over DAA scores; once the cap is
/// reached no more rewards are issued and the total supply equals the cap exactly. Payment follows acceptance order.
#[test]
fn rewards_stop_at_the_max_supply() {
    let alice = user("alice");
    let producers: Vec<User> = (0..5).map(|i| user(&format!("cap/p{i}"))).collect();
    // Halving at every DAA score: the reward at DAA score 1 is 50; genesis 100 + reward bound 200 = cap 300
    let params = Params {
        block_reward: 100,
        reward_halving_interval: 1,
        ksa_max_supply: 300,
        ..DEVNET
    };
    assert_eq!(params.max_genesis_supply(), 100);
    let mut chain = Chain::with_params(params, vec![(alice.account, 100)]);
    let producer = chain.producer;
    // 5 blue merged blocks of DAA score 1 plus the main-chain block (DAA score 1): 300 in total per block, 100 over the
    // cap
    let blues: Vec<(MergedBlock, Color)> = producers
        .iter()
        .enumerate()
        .map(|(i, p)| (merged(&format!("cap/blue/{i}"), p, vec![]), blue(1)))
        .collect();
    chain.colored_block(blues, vec![]).unwrap();
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(1, 5)]).unwrap();
    for (i, p) in producers.iter().enumerate() {
        let expected = if i < 4 { 50 } else { 0 };
        assert_eq!(chain.ksa(&p.account), expected, "merged block {i}");
    }
    assert_eq!(
        chain.ksa(&producer),
        0,
        "the main-chain block comes last, the cap is reached"
    );
    assert_eq!(supply(&chain), 300);
    assert_conserved(&chain);
    // Unlocking later blocks gives no reward either
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(2, 0)]).unwrap();
    assert_eq!(supply(&chain), 300);
    assert_conserved(&chain);
}

/// D-084 / D-090: an (empty) block of weight w takes w reward slots — the sum of the rewards of the slots at DAA scores
/// D, D−1, …, D−w+1, each slot by its own score when crossing a halving point; later empty blocks in a streak (weights
/// 24, 48, 96, 120) likewise; red blocks still get no reward. A block in the unlockable segment whose weight is neither
/// 1 nor the empty-block weight of some streak is a local fault (`Fatal`), not an invalid block.
#[test]
fn empty_blocks_take_n_reward_slots() {
    let (p1, p2) = (user("p1"), user("p2"));
    let params = Params {
        block_reward: 1_000,
        reward_halving_interval: 20,
        ..DEVNET
    };
    assert_eq!(params.empty_block_difficulty, 12);
    assert_eq!(params.max_empty_block_weight, 120);
    let mut chain = Chain::with_params(params.clone(), vec![(p1.account, 1)]);
    let producer = chain.producer;
    let empty_blue = merged("empty-blue", &p1, vec![]);
    let empty_red = merged("empty-red", &p2, vec![]);
    chain
        .colored_block(
            vec![
                (
                    empty_blue.clone(),
                    Color {
                        blue: true,
                        daa_score: 24,
                        weight: 12,
                    },
                ),
                (
                    empty_red.clone(),
                    Color {
                        blue: false,
                        daa_score: 24,
                        weight: 12,
                    },
                ),
            ],
            vec![],
        )
        .unwrap();
    assert_eq!(supply(&chain), 1);
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(1, 2)]).unwrap();
    // Slots 24-20 get 500 each, 19-13 get 1000 each; the main-chain block itself (DAA score 1, weight 1) 1000
    assert_eq!(chain.ksa(&p1.account), 1 + 5 * 500 + 7 * 1_000);
    assert_eq!(chain.ksa(&p2.account), 0, "a red block gets no reward");
    assert_eq!(chain.ksa(&producer), 1_000);
    assert_eq!(supply(&chain), 1 + 9_500 + 1_000);
    assert_conserved(&chain);

    // A later empty block in a streak: a blue block of weight 120 and DAA score 130 takes slots 11-130, crossing 6
    // halving points
    assert_eq!(
        params.block_rewards_at(130, 120),
        9 * 1_000 + 20 * (500 + 250 + 125 + 62 + 31) + 11 * 15
    );
    for (weight, daa_score) in [(24u64, 30u64), (48, 50), (96, 100), (120, 130)] {
        let mut chain = Chain::with_params(params.clone(), vec![]);
        let later = merged("later-in-streak", &p1, vec![]);
        chain
            .colored_block(
                vec![(
                    later,
                    Color {
                        blue: true,
                        daa_score,
                        weight,
                    },
                )],
                vec![],
            )
            .unwrap();
        chain.btc.settle(chain.parent);
        chain.block(vec![unlock(1, 1)]).unwrap();
        assert_eq!(
            chain.ksa(&p1.account),
            params.block_rewards_at(daa_score, weight),
            "weight {weight}"
        );
        assert_conserved(&chain);
    }

    for weight in [0u64, 5, 13, 192, 240] {
        let mut chain = Chain::with_params(params.clone(), vec![]);
        let bad = merged("bad-weight", &p1, vec![]);
        chain
            .colored_block(
                vec![(
                    bad,
                    Color {
                        blue: true,
                        daa_score: 5,
                        weight,
                    },
                )],
                vec![],
            )
            .unwrap();
        chain.btc.settle(chain.parent);
        let err = chain.block(vec![unlock(1, 1)]).unwrap_err();
        assert!(
            matches!(&err, BlockError::Fatal(m) if m.contains("weight")),
            "weight {weight}: {err:?}"
        );
    }
}

/// D-089: when the genesis config gives a block share account, every reward slot is split in two by that slot's rules:
/// a slot with DAA score below `BLOCK_SHARE_END` credits `BLOCK_SHARE_BPS / 10 000` (rounded down) to the share account
/// and the rest to the producer, later slots go entirely to the producer; a blue merged block uses its own slots, the
/// w slots of an empty block are split slot by slot; a red block gets neither; sponsored fees are not split. Without a
/// configured share account everything goes to the producer (the other tests in this file).
#[test]
fn the_block_share_goes_to_the_genesis_account() {
    let (a, holder) = (user("a"), user("share"));
    let (p1, p2, p3, p4) = (user("p1"), user("p2"), user("p3"), user("p4"));
    let params = Params {
        block_reward: 1_000,
        reward_halving_interval: 20,
        block_share_bps: 2_000,
        block_share_end: 40,
        ..DEVNET
    };
    let genesis = GenesisConfig::devnet(vec![(a.account, 1_000)])
        .with_block_share_account(Some(holder.account));
    let mut chain = Chain::with_genesis(params.clone(), genesis);
    let producer = chain.producer;
    assert_eq!(
        chain.get::<[u8; 32]>(&keys::block_share_account()),
        Some(holder.account)
    );
    assert_eq!(chain.ksa(&holder.account), 0);

    // Block 1 (DAA score 1, weight 1) collects a 7 KSA sponsored fee: after unlocking, producer 800 + 7, share account
    // 200
    chain
        .block(vec![sponsored(&a, 0, 7, GAS, transfer(p1.account, 1))])
        .unwrap();
    assert_eq!(
        supply(&chain),
        1_000,
        "rewards are paid only at unlock time"
    );
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(1, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), 800 + 7);
    assert_eq!(chain.ksa(&holder.account), 200);
    assert_eq!(supply(&chain), 1_000 + 1_000);
    assert_conserved(&chain);

    // Block 3 merges a blue empty block (DAA score 24, weight 12: slots 13-19 get 1 000 each, 20-24 get 500 each) and a
    // red empty block (no reward); block 2 (the unlocking block) and block 3 itself take one slot each
    let empty_blue = merged("share/blue", &p1, vec![]);
    let empty_red = merged("share/red", &p2, vec![]);
    chain
        .colored_block(
            vec![
                (
                    empty_blue,
                    Color {
                        blue: true,
                        daa_score: 24,
                        weight: 12,
                    },
                ),
                (
                    empty_red,
                    Color {
                        blue: false,
                        daa_score: 24,
                        weight: 12,
                    },
                ),
            ],
            vec![],
        )
        .unwrap();
    assert_eq!(chain.position(&chain.parent), (3, 2));
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(3, 2)]).unwrap();
    assert_eq!(
        params.block_rewards_split_at(24, 12),
        RewardSplit {
            producer: 7 * 750 + 5 * 375,
            share: 7 * 200 + 5 * 100,
            carrier: 7 * 50 + 5 * 25,
        },
        "without a carrier the carrier's part goes to the producer (D-095)"
    );
    assert_eq!(
        chain.ksa(&p1.account),
        1 + 7 * 800 + 5 * 400,
        "the merged blue block is split by its own slots"
    );
    assert_eq!(chain.ksa(&p2.account), 0, "a red block gets no reward");
    assert_eq!(chain.ksa(&producer), 807 + 2 * 800);
    assert_eq!(chain.ksa(&holder.account), 200 + 1_900 + 2 * 200);
    assert_eq!(supply(&chain), 2_000 + 9_500 + 2 * 1_000);
    assert_conserved(&chain);

    // The share lasts up to DAA score 40: for the blue block of DAA score 45 and weight 12 only slots 34-39 (500 each)
    // give 100 away, slots 40-45 (250 each) go entirely to the producer; the blue block of DAA score 60 (slots 49-59 at
    // 250 each, slot 60 at 125) goes entirely to the producer
    assert_eq!(
        params.block_rewards_split_at(45, 12),
        RewardSplit {
            producer: 6 * 375 + 6 * 238,
            share: 6 * 100,
            carrier: 6 * 25 + 6 * 12,
        }
    );
    let last = params.block_rewards_split_at(60, 12);
    assert_eq!((last.share, last.total()), (0, 11 * 250 + 125));
    chain
        .colored_block(
            vec![
                (
                    merged("share/ending", &p3, vec![]),
                    Color {
                        blue: true,
                        daa_score: 45,
                        weight: 12,
                    },
                ),
                (
                    merged("share/ended", &p4, vec![]),
                    Color {
                        blue: true,
                        daa_score: 60,
                        weight: 12,
                    },
                ),
            ],
            vec![],
        )
        .unwrap();
    assert_eq!(chain.position(&chain.parent), (5, 2));
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(5, 2)]).unwrap();
    assert_eq!(chain.ksa(&p3.account), 6 * 400 + 6 * 250);
    assert_eq!(chain.ksa(&p4.account), 11 * 250 + 125);
    assert_eq!(
        chain.ksa(&producer),
        807 + 4 * 800,
        "blocks 4 and 5 themselves"
    );
    assert_eq!(chain.ksa(&holder.account), 200 + 1_900 + 4 * 200 + 600);
    assert_conserved(&chain);

    // No share account configured: the same blocks go entirely to the producer
    let mut plain = Chain::with_params(params, vec![(a.account, 1_000)]);
    assert_eq!(plain.get::<[u8; 32]>(&keys::block_share_account()), None);
    plain.block(vec![]).unwrap();
    plain.btc.settle(plain.parent);
    plain.block(vec![unlock(1, 0)]).unwrap();
    assert_eq!(plain.ksa(&plain.producer), 1_000);
    assert_eq!(plain.ksa(&holder.account), 0);
    assert_conserved(&plain);
}

/// D-089 + D-074: when the supply cap applies, the share part is guaranteed first — when the last rewardable block can
/// only be paid partially, the share account takes its full part first and the producer gets the remainder.
#[test]
fn the_block_share_is_paid_first_when_the_max_supply_is_reached() {
    let (alice, holder) = (user("alice"), user("share"));
    let producers: Vec<User> = (0..5).map(|i| user(&format!("share/cap/p{i}"))).collect();
    // Halving at every DAA score: the reward at DAA score 1 is 50, share 10; genesis 100 with cap 225, the 3rd blue
    // block can only be paid 25: share 10, producer 15
    let params = Params {
        block_reward: 100,
        reward_halving_interval: 1,
        ksa_max_supply: 225,
        block_share_bps: 2_000,
        block_share_end: 10,
        ..DEVNET
    };
    let genesis = GenesisConfig::devnet(vec![(alice.account, 100)])
        .with_block_share_account(Some(holder.account));
    let mut chain = Chain::with_genesis(params, genesis);
    let producer = chain.producer;
    let blues: Vec<(MergedBlock, Color)> = producers
        .iter()
        .enumerate()
        .map(|(i, p)| (merged(&format!("share/cap/blue/{i}"), p, vec![]), blue(1)))
        .collect();
    chain.colored_block(blues, vec![]).unwrap();
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(1, 5)]).unwrap();
    for (i, p) in producers.iter().enumerate() {
        let expected = [40, 40, 15, 0, 0][i];
        assert_eq!(chain.ksa(&p.account), expected, "merged block {i}");
    }
    assert_eq!(chain.ksa(&holder.account), 30);
    assert_eq!(
        chain.ksa(&producer),
        0,
        "the main-chain block comes last, the cap is reached"
    );
    assert_eq!(supply(&chain), 225);
    assert_conserved(&chain);
    chain.btc.settle(chain.parent);
    chain.block(vec![unlock(2, 0)]).unwrap();
    assert_eq!(supply(&chain), 225);
    assert_eq!(chain.ksa(&holder.account), 30);
    assert_conserved(&chain);
}

/// Carrier reward (D-095): when a block has a carrier, `CARRIER_BPS` of every reward slot is taken from the producer's
/// part for the carrier (the block share is unchanged), and sponsored fees are split by `CARRIER_FEE_BPS`; without a
/// carrier, or when the carrier is the producer itself, everything goes to the producer; a red block has no reward,
/// only sponsored fees to split; merged blocks use their own carriers.
#[test]
fn the_carrier_takes_a_share_of_the_slots_and_of_the_fees() {
    let (a, holder, carrier) = (user("a"), user("share"), user("carrier"));
    let (p1, p2) = (user("p1"), user("p2"));
    let params = Params {
        block_reward: 1_000,
        reward_halving_interval: 100,
        block_share_bps: 2_000,
        block_share_end: 100,
        carrier_bps: 500,
        carrier_fee_bps: 5_000,
        ..DEVNET
    };
    let genesis = GenesisConfig::devnet(vec![(a.account, 1_000)])
        .with_block_share_account(Some(holder.account));
    let mut chain = Chain::with_genesis(params.clone(), genesis);
    let producer = chain.producer;
    assert_eq!(
        params.block_rewards_split_at(1, 1),
        RewardSplit {
            producer: 750,
            share: 200,
            carrier: 50,
        }
    );

    // Block 1 collects a 7 KSA sponsored fee, its carrier is carrier: producer 750 + 4 (half of 7 rounded down, 3, goes
    // to the carrier), carrier 50 + 3, share 200
    chain
        .block(vec![sponsored(&a, 0, 7, GAS, transfer(p1.account, 1))])
        .unwrap();
    let block1 = chain.parent;
    chain.btc.settle_by(block1, carrier.account);
    chain.block(vec![unlock(1, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), 750 + 4);
    assert_eq!(chain.ksa(&carrier.account), 50 + 3);
    assert_eq!(chain.ksa(&holder.account), 200);
    assert_eq!(supply(&chain), 1_000 + 1_000);
    assert_conserved(&chain);

    // Block 2 (the unlocking block, no carrier): everything to the producer; block 3's carrier is the producer itself:
    // likewise everything to the producer
    let block2 = chain.parent;
    chain.block(vec![]).unwrap();
    let block3 = chain.parent;
    chain.btc.settle(block2);
    chain.btc.settle_by(block3, producer);
    chain.block(vec![unlock(3, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), 754 + 2 * 800);
    assert_eq!(chain.ksa(&carrier.account), 53);
    assert_eq!(chain.ksa(&holder.account), 3 * 200);
    assert_conserved(&chain);

    // Block 5 merges a blue block (carrier carrier, weight 12: slots 4-15) and a red block (carrier carrier, collects a
    // 9 KSA sponsored fee: no reward, the fee is split 4 / 5)
    chain.block(vec![unlock(4, 0)]).unwrap_err();
    let blue_block = merged("carrier/blue", &p1, vec![]);
    let red_block = merged(
        "carrier/red",
        &p2,
        vec![sponsored(&a, 1, 9, GAS, transfer(p1.account, 1))],
    );
    let (blue_hash, red_hash) = (blue_block.hash, red_block.hash);
    chain
        .colored_block(
            vec![
                (
                    blue_block,
                    Color {
                        blue: true,
                        daa_score: 15,
                        weight: 12,
                    },
                ),
                (
                    red_block,
                    Color {
                        blue: false,
                        daa_score: 15,
                        weight: 1,
                    },
                ),
            ],
            vec![],
        )
        .unwrap();
    let block4 = sha256(&4u64.to_le_bytes());
    chain.btc.settle(block4);
    chain.btc.settle_by(blue_hash, carrier.account);
    chain.btc.settle_by(red_hash, carrier.account);
    chain.btc.settle(chain.parent);
    assert_eq!(chain.position(&chain.parent), (5, 2));
    chain.block(vec![unlock(5, 2)]).unwrap();
    assert_eq!(
        chain.ksa(&p1.account),
        2 + 12 * 750,
        "the blue block's producer"
    );
    assert_eq!(
        chain.ksa(&p2.account),
        5,
        "the red block's producer only gets half the sponsored fee (the half rounded up)"
    );
    assert_eq!(chain.ksa(&carrier.account), 53 + 12 * 50 + 4);
    assert_eq!(chain.ksa(&holder.account), 3 * 200 + 12 * 200 + 2 * 200);
    assert_eq!(
        chain.ksa(&producer),
        754 + 2 * 800 + 2 * 800,
        "blocks 4 and 5 have no carrier"
    );
    assert_eq!(supply(&chain), 1_000 + 5 * 1_000 + 12 * 1_000);
    assert_conserved(&chain);
}

/// When the supply cap applies, the block share is guaranteed first, then the carrier, and the producer last (D-095).
#[test]
fn the_carrier_is_paid_before_the_producer_when_the_max_supply_is_reached() {
    let (alice, holder, carrier) = (user("alice"), user("share"), user("carrier"));
    let producers: Vec<User> = (0..3).map(|i| user(&format!("carrier/cap/p{i}"))).collect();
    // 100 per slot: share 20, carrier 5, producer 75; genesis 100 with cap 260: the 1st blue block is paid in full, the
    // 2nd can only be paid 60 (share 20, carrier 5, producer 35), the 3rd nothing
    let params = Params {
        block_reward: 100,
        reward_halving_interval: 1_000,
        ksa_max_supply: 260,
        block_share_bps: 2_000,
        block_share_end: 1_000,
        carrier_bps: 500,
        ..DEVNET
    };
    let genesis = GenesisConfig::devnet(vec![(alice.account, 100)])
        .with_block_share_account(Some(holder.account));
    let mut chain = Chain::with_genesis(params, genesis);
    let producer = chain.producer;
    let blues: Vec<(MergedBlock, Color)> = producers
        .iter()
        .enumerate()
        .map(|(i, p)| (merged(&format!("carrier/cap/blue/{i}"), p, vec![]), blue(1)))
        .collect();
    let hashes: Vec<[u8; 32]> = blues.iter().map(|(m, _)| m.hash).collect();
    chain.colored_block(blues, vec![]).unwrap();
    for hash in hashes {
        chain.btc.settle_by(hash, carrier.account);
    }
    chain.btc.settle_by(chain.parent, carrier.account);
    chain.block(vec![unlock(1, 3)]).unwrap();
    for (i, p) in producers.iter().enumerate() {
        let expected = [75, 35, 0][i];
        assert_eq!(chain.ksa(&p.account), expected, "merged block {i}");
    }
    assert_eq!(chain.ksa(&holder.account), 40);
    assert_eq!(chain.ksa(&carrier.account), 10);
    assert_eq!(
        chain.ksa(&producer),
        0,
        "the main-chain block comes last, the cap is reached"
    );
    assert_eq!(supply(&chain), 260);
    assert_conserved(&chain);
}
