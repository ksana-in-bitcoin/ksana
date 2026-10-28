//! PoW: the hash byte order agrees with Bitcoin (D-025), nonce solving, block header checks (version, height, parents,
//! bits, PoW), the genesis block (D-027). These rules are independent of the mining algorithm, so the test algorithm
//! `Sha256d` is used (about 1 µs per attempt, about 1 000 times faster than the network's Argon2id); the tests of the
//! network algorithm Argon2id are in `argon2id.rs` (D-094).

use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::pow::{CompactTarget, Target};
use bitcoin::BlockHash;
use ksana_consensus::{
    check_pow, effective_target, empty_entries_root, genesis_header, pow_limit_target, pow_value,
    target_from_bits, work_from_bits, Consensus, ConsensusError, PowAlgorithm, PowConsensus,
    Sha256d, U256,
};
use ksana_primitives::hash::sha256;
use ksana_primitives::{BlockHeader, BtcAnchor, Params, Upgrade, BLOCK_VERSION, DEVNET};
use proptest::prelude::*;

const GENESIS_TIME: u64 = 1_700_000_000;

fn params_with_limit(pow_limit_bits: u32) -> Params {
    Params {
        pow_limit_bits,
        ..DEVNET
    }
}

fn genesis(params: &Params) -> BlockHeader {
    let anchor = BtcAnchor {
        height: 800_000,
        hash: [0xab; 32],
    };
    genesis_header(params, GENESIS_TIME, anchor, [0x5e; 32]).unwrap()
}

/// Consensus with the test algorithm `Sha256d` (the Argon2id selected by the network params is ignored; the injection
/// point of D-062).
fn sha256d_consensus(params: &Params, genesis: &BlockHeader) -> PowConsensus {
    PowConsensus::with_algorithm(params, genesis, Arc::new(Sha256d)).unwrap()
}

/// A child template of `parent` (unsolved, `bits` from the difficulty adjustment).
fn child(consensus: &PowConsensus, parent: &BlockHeader, timestamp: u64) -> BlockHeader {
    BlockHeader {
        version: BLOCK_VERSION,
        height: parent.height + 1,
        parents: vec![parent.hash()],
        btc_anchor: BtcAnchor {
            height: parent.btc_anchor.height + 1,
            hash: [0xcd; 32],
        },
        entries_root: [0x11; 32],
        state_root: [0x22; 32],
        producer: [0x33; 32],
        timestamp,
        bits: chain_next_bits(consensus, parent).unwrap(),
        nonce: 0,
    }
}

/// The `bits` of the block following `parent` on a pure chain (no merge set): the blocks in these tests are all
/// non-empty (weight 1), so the DAA score equals the height; `bits` is independent of this block's weight (D-084).
fn chain_next_bits(consensus: &PowConsensus, parent: &BlockHeader) -> Result<u32, ConsensusError> {
    let daa_score = parent
        .height
        .checked_add(1)
        .ok_or(ConsensusError::Overflow)?;
    consensus.next_bits(parent, daa_score, 1)
}

/// The block header check of a non-empty block (weight 1) on a pure chain: the DAA score is the parent height plus 1.
fn check_chain_header(
    consensus: &PowConsensus,
    parent: &BlockHeader,
    header: &BlockHeader,
) -> Result<(), ConsensusError> {
    let daa_score = parent
        .height
        .checked_add(1)
        .ok_or(ConsensusError::Overflow)?;
    consensus.check_header(parent, daa_score, 1, header)
}

fn meets(header: &BlockHeader, target: &U256) -> bool {
    pow_value(&header.hash()) <= *target
}

/// The Bitcoin genesis block: the raw sha256d output (internal byte order) interpreted as little-endian is the hex
/// shown by RPC / explorers taken as a big-endian number, and the real genesis block meets its own `bits` under this
/// convention; interpreted as big-endian it does not.
#[test]
fn pow_value_byte_order_matches_bitcoin() {
    let block = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin);
    let header_bytes = bitcoin::consensus::encode::serialize(&block.header);
    assert_eq!(header_bytes.len(), 80);
    let hash = ksana_primitives::hash::sha256d(&header_bytes);
    assert_eq!(hash, block.block_hash().to_byte_array());

    let display = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
    assert_eq!(block.block_hash().to_string(), display);
    let mut reversed = hash;
    reversed.reverse();
    assert_eq!(hex::encode(reversed), display);
    assert_eq!(pow_value(&hash).to_hex(), display);

    let bits = block.header.bits.to_consensus();
    assert_eq!(bits, 0x1d00_ffff);
    let target = target_from_bits(bits, 0x1d00_ffff).unwrap();
    assert!(pow_value(&hash) <= target);
    assert!(U256::from_be_bytes(hash) > target);
}

proptest! {
    /// `pow_value(h) ≤ target` agrees with rust-bitcoin's `Target::is_met_by`, including a hash exactly equal to the
    /// target and on either side of it.
    #[test]
    fn pow_check_agrees_with_rust_bitcoin(
        size in 1u32..=32,
        mantissa in 0x8000u32..=0x7f_ffff,
        delta in -2i64..=2,
        random_hash in any::<[u8; 32]>(),
    ) {
        let bits = (size << 24) | mantissa;
        prop_assume!(target_from_bits(bits, 0x2100_ffff).is_ok());
        let target = target_from_bits(bits, 0x2100_ffff).unwrap();
        let btc = Target::from_compact(CompactTarget::from_consensus(bits));
        let near = if delta >= 0 {
            target.checked_add(U256::from_u64(delta.unsigned_abs()))
        } else {
            target.checked_sub(U256::from_u64(delta.unsigned_abs()))
        };
        for hash in near.map(U256::to_le_bytes).into_iter().chain([random_hash]) {
            prop_assert_eq!(
                pow_value(&hash) <= target,
                btc.is_met_by(BlockHash::from_byte_array(hash))
            );
        }
    }
}

/// Solves and passes the check; every tampered field is rejected. Limit 0x1f00ffff (about 2^240): solving takes about
/// 2^16 attempts, and the probability that a tampered hash happens to still meet the target is about 2^-16.
#[test]
fn solved_header_passes_and_every_tampered_field_is_rejected() {
    let params = params_with_limit(0x1f00_ffff);
    let g = genesis(&params);
    let consensus = sha256d_consensus(&params, &g);
    let target = pow_limit_target(0x1f00_ffff).unwrap();

    let mut header = child(&consensus, &g, GENESIS_TIME + 5);
    assert_eq!(
        header.bits, 0x1f00_ffff,
        "block on schedule, difficulty unchanged"
    );
    assert!(consensus.solve(&mut header, 1, 0, 10_000_000).unwrap());
    assert!(meets(&header, &target));
    assert_eq!(check_chain_header(&consensus, &g, &header), Ok(()));
    assert_eq!(
        check_pow(&Sha256d, &header, params.pow_limit_bits, 1),
        Ok(())
    );
    // sequential search: what is found is the first nonce in the range that meets the target
    let first = (0..=header.nonce)
        .find(|&n| {
            meets(
                &BlockHeader {
                    nonce: n,
                    ..header.clone()
                },
                &target,
            )
        })
        .unwrap();
    assert_eq!(first, header.nonce);

    let check = |f: &dyn Fn(&mut BlockHeader)| {
        let mut tampered = header.clone();
        f(&mut tampered);
        check_chain_header(&consensus, &g, &tampered)
    };
    // version 1 is the block header of the linear chain (before D-063)
    assert_eq!(
        check(&|h| h.version = 1),
        Err(ConsensusError::BadVersion {
            expected: 2,
            got: 1
        })
    );
    // higher than the highest version this software knows is an unknown upgrade (D-076)
    assert_eq!(
        check(&|h| h.version = 3),
        Err(ConsensusError::UnknownVersion {
            got: 3,
            max_known: 2
        })
    );
    assert_eq!(
        check(&|h| h.version = 0),
        Err(ConsensusError::BadVersion {
            expected: 2,
            got: 0
        })
    );
    assert_eq!(
        check(&|h| h.height = 2),
        Err(ConsensusError::BadHeight {
            expected: 1,
            got: 2
        })
    );
    assert_eq!(
        check(&|h| h.height = 0),
        Err(ConsensusError::BadHeight {
            expected: 1,
            got: 0
        })
    );
    assert_eq!(
        check(&|h| h.parents[0][31] ^= 1),
        Err(ConsensusError::BadParent)
    );
    assert_eq!(
        check(&|h| h.parents = vec![[0; 32]]),
        Err(ConsensusError::BadParent)
    );
    assert_eq!(
        check(&|h| h.parents.clear()),
        Err(ConsensusError::BadParent)
    );
    // the parents other than the selected parent: strictly ascending, not containing the selected parent (the
    // remaining structural rules are checked by GHOSTDAG, D-063)
    assert_eq!(
        check(&|h| h.parents.extend([[9; 32], [1; 32]])),
        Err(ConsensusError::BadParentOrder)
    );
    assert_eq!(
        check(&|h| h.parents.extend([[1; 32], [1; 32]])),
        Err(ConsensusError::BadParentOrder)
    );
    assert_eq!(
        check(&|h| {
            let sp = h.parents[0];
            h.parents.push(sp);
        }),
        Err(ConsensusError::BadParentOrder)
    );
    assert_eq!(
        check(&|h| h.bits = 0x1f00_fffe),
        Err(ConsensusError::BitsMismatch {
            expected: 0x1f00_ffff,
            got: 0x1f00_fffe
        })
    );
    assert_eq!(
        check(&|h| h.bits = 0x207f_ffff),
        Err(ConsensusError::BitsMismatch {
            expected: 0x1f00_ffff,
            got: 0x207f_ffff
        })
    );
    // the remaining fields only enter the hash: the PoW is no longer met
    let pow_only: [&dyn Fn(&mut BlockHeader); 10] = [
        &|h| h.btc_anchor.height += 1,
        &|h| h.btc_anchor.hash[0] ^= 1,
        &|h| h.entries_root[5] ^= 0x80,
        &|h| h.state_root[31] ^= 1,
        &|h| h.producer[0] ^= 1,
        &|h| h.timestamp += 1,
        &|h| h.timestamp -= 1,
        &|h| h.nonce += 1,
        &|h| h.nonce ^= 1 << 63,
        &|h| h.nonce = h.nonce.wrapping_sub(1),
    ];
    for (i, f) in pow_only.iter().enumerate() {
        assert_eq!(
            check(*f),
            Err(ConsensusError::InsufficientPow),
            "tamper #{i}"
        );
    }
}

/// `bits` must be bit-for-bit identical to the result of the difficulty adjustment: an easier valid target, and a
/// non-canonical encoding of the same target, are rejected even when the PoW is met.
#[test]
fn wrong_bits_are_rejected_even_with_valid_pow() {
    // devnet: the parent is 5 seconds ahead of schedule, the child's target is slightly below the limit
    let g = genesis(&DEVNET);
    let consensus = sha256d_consensus(&DEVNET, &g);
    let mut b1 = child(&consensus, &g, GENESIS_TIME);
    assert!(consensus.solve(&mut b1, 1, 0, 1000).unwrap());
    assert_eq!(check_chain_header(&consensus, &g, &b1), Ok(()));
    let expected = chain_next_bits(&consensus, &b1).unwrap();
    assert_ne!(expected, DEVNET.pow_limit_bits);
    assert!(
        target_from_bits(expected, DEVNET.pow_limit_bits).unwrap()
            < pow_limit_target(DEVNET.pow_limit_bits).unwrap()
    );
    let mut b2 = child(&consensus, &b1, GENESIS_TIME + 3);
    b2.bits = DEVNET.pow_limit_bits; // easier, but valid in itself
    assert!(consensus.solve(&mut b2, 1, 0, 1000).unwrap());
    assert_eq!(check_pow(&Sha256d, &b2, DEVNET.pow_limit_bits, 1), Ok(()));
    assert_eq!(
        check_chain_header(&consensus, &b1, &b2),
        Err(ConsensusError::BitsMismatch {
            expected,
            got: DEVNET.pow_limit_bits
        })
    );

    // non-canonical encoding: 0x1f7fff00 and 0x20007fff represent the same target
    let params = params_with_limit(0x1f7f_ff00);
    let g = genesis(&params);
    let consensus = sha256d_consensus(&params, &g);
    assert_eq!(
        target_from_bits(0x2000_7fff, params.pow_limit_bits),
        target_from_bits(0x1f7f_ff00, params.pow_limit_bits)
    );
    let mut header = child(&consensus, &g, GENESIS_TIME + 5);
    assert_eq!(header.bits, 0x1f7f_ff00);
    let mut non_canonical = header.clone();
    non_canonical.bits = 0x2000_7fff;
    assert!(consensus
        .solve(&mut non_canonical, 1, 0, 1_000_000)
        .unwrap());
    assert_eq!(
        check_pow(&Sha256d, &non_canonical, params.pow_limit_bits, 1),
        Ok(())
    );
    assert_eq!(
        check_chain_header(&consensus, &g, &non_canonical),
        Err(ConsensusError::BitsMismatch {
            expected: 0x1f7f_ff00,
            got: 0x2000_7fff
        })
    );
    assert!(consensus.solve(&mut header, 1, 0, 1_000_000).unwrap());
    assert_eq!(check_chain_header(&consensus, &g, &header), Ok(()));
}

/// [`Consensus::check_header`] is [`Consensus::check_header_rules`] followed by [`Consensus::check_header_pow`]. The
/// PoW part needs only the header and its weight, so block validation checks it before GHOSTDAG (second audit, P1-1);
/// it checks the header's own `bits` (the rules then require them to be the expected ones) and, like `check_header`,
/// rejects an unknown version before the PoW, whose rules an upgrade may have changed (D-076).
#[test]
fn the_pow_part_of_the_header_check_needs_only_the_header() {
    let g = genesis(&DEVNET);
    let consensus = sha256d_consensus(&DEVNET, &g);
    let mut b1 = child(&consensus, &g, GENESIS_TIME);
    assert!(consensus.solve(&mut b1, 1, 0, 1000).unwrap());
    assert_eq!(consensus.check_header_pow(&b1, 1), Ok(()));
    assert_eq!(consensus.check_header_rules(&g, 1, 1, &b1), Ok(()));

    // a nonce missing the target: only the PoW part fails
    let mut weak = b1.clone();
    while consensus.check_header_pow(&weak, 1).is_ok() {
        weak.nonce += 1;
    }
    assert_eq!(
        consensus.check_header_pow(&weak, 1),
        Err(ConsensusError::InsufficientPow)
    );
    assert_eq!(consensus.check_header_rules(&g, 1, 1, &weak), Ok(()));
    assert_eq!(
        check_chain_header(&consensus, &g, &weak),
        Err(ConsensusError::InsufficientPow)
    );

    // easier `bits` than the expected ones, with PoW for them: the PoW part passes, the rules reject the bits
    let expected = chain_next_bits(&consensus, &b1).unwrap();
    let mut easy = child(&consensus, &b1, GENESIS_TIME + 3);
    easy.bits = DEVNET.pow_limit_bits;
    assert_ne!(easy.bits, expected);
    assert!(consensus.solve(&mut easy, 1, 0, 1000).unwrap());
    assert_eq!(consensus.check_header_pow(&easy, 1), Ok(()));
    assert_eq!(
        consensus.check_header_rules(&b1, 2, 1, &easy),
        Err(ConsensusError::BitsMismatch {
            expected,
            got: DEVNET.pow_limit_bits
        })
    );

    // an unknown version: rejected as such before the PoW
    let mut future = weak.clone();
    future.version = 3;
    assert_eq!(
        consensus.check_header_pow(&future, 1),
        Err(ConsensusError::UnknownVersion {
            got: 3,
            max_known: 2
        })
    );
}

#[test]
fn solve_searches_exactly_the_given_range() {
    let params = params_with_limit(0x1f0f_ffff); // success rate about 2^-12
    let g = genesis(&params);
    let consensus = sha256d_consensus(&params, &g);
    let target = pow_limit_target(params.pow_limit_bits).unwrap();
    let template = child(&consensus, &g, GENESIS_TIME + 5);
    let brute_force = |range: std::ops::RangeInclusive<u64>| {
        range.into_iter().find(|&n| {
            meets(
                &BlockHeader {
                    nonce: n,
                    ..template.clone()
                },
                &target,
            )
        })
    };

    for (start, iterations) in [
        (0u64, 20_000u64),
        (5_000, 3_000),
        (1 << 40, 20_000),
        (123, 1),
    ] {
        let mut header = template.clone();
        let found = consensus.solve(&mut header, 1, start, iterations).unwrap();
        let expected = brute_force(start..=start + iterations - 1);
        assert_eq!(found, expected.is_some(), "start {start}");
        assert_eq!(
            header.nonce,
            expected.unwrap_or(template.nonce),
            "start {start}"
        );
    }
    // iterations = 0: does nothing
    let mut header = template.clone();
    assert!(!consensus.solve(&mut header, 1, 0, 0).unwrap());
    assert_eq!(header, template);
    // up to u64::MAX, no wrap-around
    for iterations in [1, 4, 1000, u64::MAX] {
        let mut header = template.clone();
        let found = consensus
            .solve(&mut header, 1, u64::MAX - 3, iterations)
            .unwrap();
        let expected = brute_force(u64::MAX - 3..=(u64::MAX - 3).saturating_add(iterations - 1));
        assert_eq!(found, expected.is_some());
        assert_eq!(header.nonce, expected.unwrap_or(template.nonce));
    }
    // target 1: impossible to find, header unchanged
    let mut header = template.clone();
    header.bits = 0x0101_0000;
    let before = header.clone();
    assert!(!consensus.solve(&mut header, 1, 0, 2000).unwrap());
    assert_eq!(header, before);
    // invalid bits (negative, above the limit): returns false right away
    for bits in [0x0480_0000, 0x207f_ffff] {
        let mut header = template.clone();
        header.bits = bits;
        assert!(!consensus.solve(&mut header, 1, 0, 100).unwrap());
        assert_eq!(header.nonce, template.nonce);
    }
    // the algorithm can solve directly against any target
    let mut header = template.clone();
    assert!(Sha256d.solve(&mut header, &U256::MAX, 77, 1).unwrap());
    assert_eq!(header.nonce, 77);
}

#[test]
fn check_header_rejects_invalid_parent_height_and_bits() {
    let g = genesis(&DEVNET);
    let consensus = sha256d_consensus(&DEVNET, &g);
    // the parent height is already u64::MAX
    let parent = BlockHeader {
        height: u64::MAX,
        ..g.clone()
    };
    let header = BlockHeader {
        height: 0,
        parents: vec![parent.hash()],
        ..g.clone()
    };
    assert_eq!(
        consensus.check_header(&parent, u64::MAX, 1, &header),
        Err(ConsensusError::Overflow)
    );
    // invalid bits (negative) are rejected before the PoW for not matching the difficulty adjustment
    let mut header = child(&consensus, &g, GENESIS_TIME + 5);
    header.bits = 0x2080_0000;
    assert_eq!(
        check_chain_header(&consensus, &g, &header),
        Err(ConsensusError::BitsMismatch {
            expected: DEVNET.pow_limit_bits,
            got: 0x2080_0000
        })
    );
    assert_eq!(
        check_pow(&Sha256d, &header, DEVNET.pow_limit_bits, 1),
        Err(ConsensusError::InvalidBits(0x2080_0000))
    );
}

/// D-027: the fields of the genesis block header; the work of the devnet genesis block (empty block, weight 12) is 24
/// (D-084).
#[test]
fn genesis_header_follows_d027() {
    let anchor = BtcAnchor {
        height: 123,
        hash: [0x77; 32],
    };
    let g = genesis_header(&DEVNET, 1_234_567, anchor.clone(), [0x99; 32]).unwrap();
    assert_eq!(g.version, BLOCK_VERSION);
    assert_eq!(g.version, 2);
    assert_eq!(g.height, 0);
    assert!(g.parents.is_empty());
    assert_eq!(g.btc_anchor, anchor);
    assert_eq!(g.entries_root, ksana_crypto::entries_root(&[]).unwrap());
    assert_eq!(g.entries_root, sha256(b""));
    assert_eq!(empty_entries_root(), g.entries_root);
    assert_eq!(
        hex::encode(g.entries_root),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(g.state_root, [0x99; 32]);
    assert_eq!(g.producer, [0; 32]);
    assert_eq!(g.timestamp, 1_234_567);
    assert_eq!(g.bits, DEVNET.pow_limit_bits);
    assert_eq!(g.nonce, 0);

    let consensus = sha256d_consensus(&DEVNET, &g);
    // the genesis block is an empty block (streak 1, weight 12, D-084): the work by the effective target (1/12 of the
    // bits target) is 12 × 2 = 24; the bits itself corresponds to work 2
    assert_eq!(consensus.streak(&g, 0), 1);
    assert_eq!(consensus.weight(&g, 0), 12);
    assert_eq!(consensus.genesis_work(&g), Ok(U256::from_u64(24)));
    assert_eq!(consensus.work(&g, 1), Ok(U256::from_u64(2)));
    assert_eq!(consensus.params(), &DEVNET);
    assert_eq!(
        consensus.asert().anchor_parent_time(),
        1_234_567 - i128::from(DEVNET.target_block_time_secs)
    );
    assert_eq!(
        work_from_bits(g.bits, DEVNET.pow_limit_bits),
        Ok(U256::from_u64(2))
    );

    let bad = params_with_limit(0x2080_0000);
    assert_eq!(
        genesis_header(&bad, 0, anchor, [0; 32]),
        Err(ConsensusError::InvalidPowLimit(0x2080_0000))
    );
}

#[test]
fn pow_consensus_new_validates_genesis_and_params() {
    let g = genesis(&DEVNET);
    assert_eq!(
        PowConsensus::new(
            &DEVNET,
            &BlockHeader {
                height: 3,
                ..g.clone()
            }
        ),
        Err(ConsensusError::BadHeight {
            expected: 0,
            got: 3
        })
    );
    assert_eq!(
        PowConsensus::new(
            &DEVNET,
            &BlockHeader {
                bits: 0x2100_8000,
                ..g.clone()
            }
        ),
        Err(ConsensusError::InvalidBits(0x2100_8000))
    );
    assert!(matches!(
        PowConsensus::new(
            &Params {
                asert_half_life_secs: 0,
                ..DEVNET
            },
            &g
        ),
        Err(ConsensusError::InvalidParams(_))
    ));
    assert!(matches!(
        PowConsensus::new(
            &Params {
                target_block_time_secs: 0,
                ..DEVNET
            },
            &g
        ),
        Err(ConsensusError::InvalidParams(_))
    ));
}

/// Usable as a trait object across threads (SPEC §6: consensus is replaceable); work is computed from bits, the chain
/// with the greatest cumulative work wins.
#[test]
fn consensus_is_a_replaceable_trait_object() {
    fn assert_send_sync<T: Send + Sync>(_: &T) {}
    let g = genesis(&DEVNET);
    let consensus: Box<dyn Consensus> = Box::new(sha256d_consensus(&DEVNET, &g));
    assert_eq!(consensus.solve_chunk(), 1 << 14);
    assert_send_sync(&consensus);

    // a chain whose timestamps all equal the genesis time (each block T seconds ahead of schedule, 4 500 seconds ahead
    // in all): every block passes the check, the difficulty rises block by block, and the work
    // ⌊2^256 / (target + 1)⌋ does not decrease; after about 4 212 seconds ahead (log2(1.5) half-lives, half-life
    // 7 200 seconds, D-090) the target drops below 2^256 / 3 and the work becomes 3
    let t = DEVNET.target_block_time_secs;
    let blocks = 4_500 / t;
    let pow = sha256d_consensus(&DEVNET, &g);
    let mut chain = vec![g.clone()];
    // the genesis block is an empty block (weight 12, D-084); its work is computed separately and not counted
    let mut works = Vec::new();
    for _ in 1..=blocks {
        let parent = chain.last().unwrap();
        let mut header = child(&pow, parent, GENESIS_TIME);
        // pure chain: daa_score equals the height
        header.bits = consensus.next_bits(parent, header.height, 1).unwrap();
        assert!(consensus.solve(&mut header, 1, 0, 10_000).unwrap());
        assert_eq!(
            consensus.check_header(parent, header.height, 1, &header),
            Ok(())
        );
        works.push(consensus.work(&header, 1).unwrap());
        chain.push(header);
    }
    assert!(works.windows(2).all(|w| w[0] <= w[1]));
    // the genesis block is on time by definition and block 1's difficulty is unchanged; from then on each block is T
    // seconds ahead and the target strictly decreases
    assert_eq!(chain[1].bits, g.bits);
    assert!(chain[1..].windows(2).all(|w| {
        target_from_bits(w[1].bits, DEVNET.pow_limit_bits).unwrap()
            < target_from_bits(w[0].bits, DEVNET.pow_limit_bits).unwrap()
    }));
    let threes = works.iter().filter(|&&w| w == U256::from_u64(3)).count() as u64;
    let twos = works.iter().filter(|&&w| w == U256::from_u64(2)).count() as u64;
    assert_eq!(twos + threes, blocks);
    // the block at height h is T·(h − 1) seconds ahead (its parent is T·(h−1) ahead), target ≈ limit·2^(−T(h−1)/7200)
    // < 2^256/3 ⇔ T(h − 1) > 4 212: the first block with work 3 (height twos + 1) is at the first multiple of T after
    // 4 212 seconds ahead
    let lead = t * twos;
    assert!(
        (4_212..4_212 + t + 4).contains(&lead),
        "the work becomes 3 at {lead} seconds ahead"
    );
    let total = works
        .iter()
        .try_fold(U256::ZERO, |acc, w| acc.checked_add(*w))
        .unwrap();
    assert_eq!(total, U256::from_u64(2 * twos + 3 * threes));
}

/// D-076: the block header version is checked against the upgrade table at the block's DAA score (the height on a
/// linear chain): before the activation point it must be the old version, after it the new version.
#[test]
fn header_version_follows_the_upgrade_schedule() {
    const UPGRADES: &[Upgrade] = &[Upgrade::new(3, 3)];
    let params = Params {
        upgrades: UPGRADES,
        ..DEVNET
    };
    let g = genesis(&params);
    let consensus = sha256d_consensus(&params, &g);
    let mut parent = g;
    for height in 1..=4u64 {
        let expected = if height < 3 { 2 } else { 3 };
        assert_eq!(params.protocol_version(height), expected);
        let mut header = child(&consensus, &parent, GENESIS_TIME + height);
        for wrong in [1, 2, 3, 4].into_iter().filter(|v| *v != expected) {
            let mut bad = BlockHeader {
                version: wrong,
                ..header.clone()
            };
            assert!(consensus.solve(&mut bad, 1, 0, 1_000_000).unwrap());
            let error = if wrong > 3 {
                ConsensusError::UnknownVersion {
                    got: wrong,
                    max_known: 3,
                }
            } else {
                ConsensusError::BadVersion {
                    expected,
                    got: wrong,
                }
            };
            assert_eq!(check_chain_header(&consensus, &parent, &bad), Err(error));
        }
        header.version = expected;
        assert!(consensus.solve(&mut header, 1, 0, 1_000_000).unwrap());
        assert_eq!(check_chain_header(&consensus, &parent, &header), Ok(()));
        parent = header;
    }
}

/// D-084 two-speed difficulty: an empty block after a non-empty block (`entries_root` is the root of the empty entry
/// list) has weight `EMPTY_BLOCK_DIFFICULTY` = 12, its effective target is 1/12 of the `bits` target and its work
/// about 12 times; consecutive empty blocks double their weight up to `MAX_EMPTY_BLOCK_WEIGHT` = 120 (D-090); `bits`
/// is determined by the selected parent and the DAA score without this block's weight, independent of whether this
/// block is empty or how long the streak is; an empty block whose PoW only meets the non-empty target is invalid; the
/// block header is invalid when the target divided by the weight is 0 (an error is returned, no panic).
#[test]
fn empty_blocks_need_n_times_the_work_at_the_same_bits() {
    let params = params_with_limit(0x1f00_ffff);
    let g = genesis(&params);
    let consensus = sha256d_consensus(&params, &g);
    assert_eq!(params.empty_block_difficulty, 12);
    assert_eq!(params.max_empty_block_weight, 120);
    let limit = params.pow_limit_bits;
    let target = pow_limit_target(limit).unwrap();
    let effective = effective_target(limit, limit, 12).unwrap();
    assert_eq!(effective, target.checked_div(U256::from_u64(12)).unwrap());
    assert_eq!(effective_target(limit, limit, 1), Ok(target));

    let full = child(&consensus, &g, GENESIS_TIME + 5);
    let mut empty = BlockHeader {
        entries_root: empty_entries_root(),
        ..full.clone()
    };
    // weight and streak: a non-empty block is always 1, streak 0; an empty block is 12, 24, 48, 96, 120, 120… for the
    // selected parent's streak 0, 1, 2, 3, 4, …
    for sp_streak in [0u64, 1, 4, 9] {
        assert_eq!(consensus.weight(&full, sp_streak), 1);
        assert_eq!(consensus.streak(&full, sp_streak), 0);
        assert_eq!(consensus.streak(&empty, sp_streak), sp_streak + 1);
    }
    assert_eq!(
        (0..7u64)
            .map(|s| consensus.weight(&empty, s))
            .collect::<Vec<_>>(),
        [12, 24, 48, 96, 120, 120, 120]
    );
    // work by weight: effective target = target / weight; the target 65535·2^224 is divisible by 12, 24 and 120, so
    // the work is exactly a multiple of 65537
    assert_eq!(consensus.work(&empty, 24), Ok(U256::from_u64(24 * 65_537)));
    assert_eq!(
        consensus.work(&empty, 120),
        Ok(U256::from_u64(120 * 65_537))
    );
    // an empty and a non-empty block after the same parent have the same bits: the empty block's DAA score includes
    // its own weight 12
    assert_eq!(
        consensus.next_bits(&g, 12, 12),
        consensus.next_bits(&g, 1, 1)
    );
    for (daa_score, timestamp) in [
        (1u64, GENESIS_TIME),
        (7, GENESIS_TIME + 40),
        (1_000, GENESIS_TIME + 4_000),
    ] {
        let sp = BlockHeader {
            timestamp,
            ..g.clone()
        };
        assert_eq!(
            consensus.next_bits(&sp, daa_score + 11, 12),
            consensus.next_bits(&sp, daa_score, 1)
        );
    }
    // work: target 65535·2^224, ⌊2^256 / (target + 1)⌋ = 65537; effective target 21845·2^222, work 786444 = 12 × 65537
    assert_eq!(consensus.work(&full, 1), Ok(U256::from_u64(65_537)));
    assert_eq!(consensus.work(&empty, 12), Ok(U256::from_u64(786_444)));

    // solving the empty block: the solution meets the effective target and passes the check with DAA score 12 and
    // weight 12; checked with DAA score 1 (without the weight), removing the weight overflows
    assert!(consensus.solve(&mut empty, 12, 0, 100_000_000).unwrap());
    assert!(meets(&empty, &effective));
    assert_eq!(consensus.check_header(&g, 12, 12, &empty), Ok(()));
    assert_eq!(check_pow(&Sha256d, &empty, limit, 12), Ok(()));
    assert_eq!(
        consensus.check_header(&g, 1, 12, &empty),
        Err(ConsensusError::Overflow)
    );

    // an empty block meeting only the non-empty target: the PoW value is within (effective target, target], met with
    // weight 1 and not met with weight 12, the block header is invalid
    let mut weak = BlockHeader {
        entries_root: empty_entries_root(),
        ..full.clone()
    };
    let mut start = 0;
    loop {
        assert!(Sha256d
            .solve(&mut weak, &target, start, 100_000_000)
            .unwrap());
        if !meets(&weak, &effective) {
            break;
        }
        start = weak.nonce + 1;
    }
    assert_eq!(check_pow(&Sha256d, &weak, limit, 1), Ok(()));
    assert_eq!(
        check_pow(&Sha256d, &weak, limit, 12),
        Err(ConsensusError::InsufficientPow)
    );
    assert_eq!(
        consensus.check_header(&g, 12, 12, &weak),
        Err(ConsensusError::InsufficientPow)
    );
    // the same PoW is even less sufficient for a higher streak (weight 24)
    assert_eq!(
        consensus.check_header(&g, 24, 24, &weak),
        Err(ConsensusError::InsufficientPow)
    );

    // target too small: target 1 divided by 12 is 0, there is no effective target; likewise for weight 0; solving
    // fails right away and the block header is unchanged
    let too_small = |weight: u64| ConsensusError::TargetTooSmall {
        bits: 0x0101_0000,
        weight,
    };
    assert_eq!(effective_target(0x0101_0000, limit, 12), Err(too_small(12)));
    assert_eq!(effective_target(0x0101_0000, limit, 1), Ok(U256::ONE));
    assert_eq!(effective_target(0x0101_0000, limit, 0), Err(too_small(0)));
    let tiny = BlockHeader {
        bits: 0x0101_0000,
        ..empty.clone()
    };
    let mut unsolved = tiny.clone();
    assert!(!consensus.solve(&mut unsolved, 12, 0, 1_000).unwrap());
    assert_eq!(unsolved, tiny);
    assert_eq!(consensus.work(&tiny, 12), Err(too_small(12)));
    assert_eq!(check_pow(&Sha256d, &tiny, limit, 12), Err(too_small(12)));
}

/// D-090: a run of empty blocks with weights 12, 24, 48, 96, 120, 120…: each is solved and checked with its own
/// weight, `bits` does not change with the streak (with equal parent timestamps it changes block by block only because
/// the DAA score advances), and the work scales with the weight; a block in the streak checked with the weight one
/// level lower most likely has insufficient PoW (the solutions used here only barely meet their own effective target);
/// a non-empty block ends the streak, and a later empty block starts again from 12.
#[test]
fn consecutive_empty_blocks_double_their_weight_up_to_the_cap() {
    let params = params_with_limit(0x1f7f_ffff); // limit about 2^247: an empty block of weight 120 needs ~2^16 attempts
    let g = genesis(&params);
    let consensus = sha256d_consensus(&params, &g);
    let limit = params.pow_limit_bits;
    let mut parent = g.clone();
    let (mut daa_score, mut streak) = (0u64, 0u64);
    let mut weights = Vec::new();
    for i in 1..=8u64 {
        let empty = i != 7; // block 7 is non-empty: ends the streak
        let mut header = child(&consensus, &parent, GENESIS_TIME + 5 * i);
        if empty {
            header.entries_root = empty_entries_root();
        }
        let weight = consensus.weight(&header, streak);
        let next_streak = consensus.streak(&header, streak);
        daa_score += weight;
        header.bits = consensus.next_bits(&parent, daa_score, weight).unwrap();
        assert_eq!(
            header.bits,
            consensus
                .next_bits(&parent, daa_score - weight + 1, 1)
                .unwrap(),
            "bits is independent of this block's weight"
        );
        let target = effective_target(header.bits, limit, weight).unwrap();
        // find a solution that only barely meets the effective target: the PoW value is within (target / 2, target]
        // (no longer met once the weight doubles)
        let mut start = 0;
        loop {
            assert!(consensus
                .solve(&mut header, weight, start, 1 << 24)
                .unwrap());
            if pow_value(&header.hash()) > target.checked_div(U256::from_u64(2)).unwrap() {
                break;
            }
            start = header.nonce + 1;
        }
        assert_eq!(
            consensus.check_header(&parent, daa_score, weight, &header),
            Ok(()),
            "block {i}"
        );
        assert_eq!(check_pow(&Sha256d, &header, limit, weight), Ok(()));
        if weight < 120 {
            let doubled = weight * 2;
            assert_eq!(
                check_pow(&Sha256d, &header, limit, doubled),
                Err(ConsensusError::InsufficientPow),
                "block {i} checked with weight {doubled}"
            );
        }
        // the work scales with the weight: the effective target is an integer division, so it differs from weight
        // times the weight-1 work by at most weight
        let scaled = work_from_bits(header.bits, limit)
            .unwrap()
            .checked_mul_u64(weight)
            .unwrap();
        let work = consensus.work(&header, weight).unwrap();
        let slack = U256::from_u64(weight);
        assert!(
            work >= scaled.checked_sub(slack).unwrap()
                && work <= scaled.checked_add(slack).unwrap(),
            "work {work:?} of block {i} should be about {weight} times the weight-1 work, {scaled:?}"
        );
        weights.push(weight);
        streak = next_streak;
        parent = header;
    }
    assert_eq!(weights, [12, 24, 48, 96, 120, 120, 1, 12]);
    assert_eq!(streak, 1);
    assert_eq!(daa_score, 12 + 24 + 48 + 96 + 120 + 120 + 1 + 12);
}
