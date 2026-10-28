//! The behavior of ASERT anchored at the genesis block (D-026): the difficulty is unchanged when blocks arrive on
//! schedule, the target exactly halves / doubles per half-life ahead of / behind schedule, clamping at both ends,
//! monotonicity, and the parameter checks.

use ksana_consensus::{
    asert_target, bits_from_target, genesis_header, pow_limit_target, target_from_bits, Asert,
    ConsensusError, U256,
};
use ksana_primitives::{BlockHeader, BtcAnchor, Params, DEVNET};
use proptest::prelude::*;

const GENESIS_TIME: u64 = 1_700_000_000;
const T: u64 = 5; // DEVNET.target_block_time_secs
const TAU: u64 = 7200; // DEVNET.asert_half_life_secs (D-090)

fn genesis(params: &Params) -> BlockHeader {
    let anchor = BtcAnchor {
        height: 100,
        hash: [7; 32],
    };
    genesis_header(params, GENESIS_TIME, anchor, [9; 32]).unwrap()
}

fn devnet_asert() -> Asert {
    Asert::from_genesis(&DEVNET, &genesis(&DEVNET)).unwrap()
}

/// The on-schedule parent timestamp: the block at height h is h·T seconds after the genesis block.
fn on_schedule(height: u64) -> u64 {
    GENESIS_TIME + height * T
}

#[test]
fn devnet_parameters_are_as_specified() {
    assert_eq!(
        (DEVNET.target_block_time_secs, DEVNET.asert_half_life_secs),
        (T, TAU)
    );
    let asert = devnet_asert();
    assert_eq!(asert.anchor_parent_time(), i128::from(GENESIS_TIME - T));
    assert_eq!(
        asert.anchor_target(),
        pow_limit_target(DEVNET.pow_limit_bits).unwrap()
    );
    assert_eq!(asert.pow_limit(), asert.anchor_target());
}

#[test]
fn on_schedule_keeps_the_anchor_target() {
    let asert = devnet_asert();
    for height in [0, 1, 2, 119, 120, 1000, 1_000_000, 1 << 40] {
        assert_eq!(
            asert.next_bits(height, on_schedule(height)),
            Ok(DEVNET.pow_limit_bits),
            "height {height}"
        );
    }
    // likewise unchanged when the anchor is harder than the limit
    let mut g = genesis(&DEVNET);
    g.bits = 0x1e01_2345;
    let asert = Asert::from_genesis(&DEVNET, &g).unwrap();
    for height in [0, 7, 100_000] {
        assert_eq!(
            asert.next_bits(height, on_schedule(height)),
            Ok(0x1e01_2345)
        );
    }
}

/// A whole number of half-lives ahead of schedule gives frac = 0, factor = 65536, and the result is the anchor target
/// times an exact power of 2.
#[test]
fn whole_half_lives_scale_the_target_exactly() {
    let limit = pow_limit_target(DEVNET.pow_limit_bits).unwrap();
    let asert = devnet_asert();
    let height = 10_000;
    for k in 1..=10u32 {
        let ahead = on_schedule(height) - u64::from(k) * TAU;
        assert_eq!(
            asert.next_target(height, ahead),
            Ok(limit.shr(k)),
            "{k} half-lives"
        );
    }
    // one half-life ahead: the devnet limit 0x7fffff·2^232 halves
    assert_eq!(
        asert.next_bits(height, on_schedule(height) - TAU),
        Ok(0x203f_ffff)
    );

    // a harder anchor: k half-lives behind schedule, the target is multiplied by exactly 2^k
    let mut g = genesis(&DEVNET);
    g.bits = 0x1f00_ffff;
    let anchor = target_from_bits(g.bits, DEVNET.pow_limit_bits).unwrap();
    let asert = Asert::from_genesis(&DEVNET, &g).unwrap();
    for k in 1..=15u32 {
        let behind = on_schedule(height) + u64::from(k) * TAU;
        assert_eq!(asert.next_target(height, behind), Ok(anchor.shl(k)));
    }
    // 0xffff·2^(224+15) = 0x7fff80·2^232 is still below the limit 0x7fffff·2^232; one more half-life behind takes the
    // limit
    assert!(anchor.shl(15) < limit && anchor.shl(16) > limit);
    assert_eq!(
        asert.next_target(height, on_schedule(height) + 16 * TAU),
        Ok(limit)
    );
}

#[test]
fn clamps_to_pow_limit_and_to_one() {
    let limit = pow_limit_target(DEVNET.pow_limit_bits).unwrap();
    let asert = devnet_asert();
    // far behind schedule (or a huge parent timestamp): not above the limit
    assert_eq!(asert.next_target(5, u64::MAX), Ok(limit));
    assert_eq!(asert.next_target(0, GENESIS_TIME + 10 * TAU), Ok(limit));
    // far ahead of schedule: the minimum is 1 (bits 0x01010000)
    assert_eq!(asert.next_target(1 << 40, GENESIS_TIME), Ok(U256::ONE));
    assert_eq!(asert.next_bits(u64::MAX, 0), Ok(0x0101_0000));
    // right around the saturation boundary: 255 half-lives ahead = limit >> 255 = 0 (floor) → 1
    let height = 255 * TAU / T + 1;
    assert_eq!(
        asert.next_target(height, on_schedule(height) - 254 * TAU),
        Ok(limit.shr(254))
    );
    assert_eq!(
        asert.next_target(height, on_schedule(height) - 255 * TAU),
        Ok(U256::ONE)
    );
}

/// When the genesis timestamp is less than T the anchor parent time is negative, and the computation proceeds as
/// usual.
#[test]
fn negative_anchor_parent_time() {
    let mut g = genesis(&DEVNET);
    g.timestamp = 3;
    let asert = Asert::from_genesis(&DEVNET, &g).unwrap();
    assert_eq!(asert.anchor_parent_time(), -2);
    assert_eq!(asert.next_bits(0, 3), Ok(DEVNET.pow_limit_bits));
    let limit = pow_limit_target(DEVNET.pow_limit_bits).unwrap();
    // parent height 1440, timestamp 3 (7 200 seconds ahead = one half-life)
    assert_eq!(asert.next_target(1440, 3), Ok(limit.shr(1)));
}

#[test]
fn from_genesis_validates_its_inputs() {
    let g = genesis(&DEVNET);
    let mut bad = g.clone();
    bad.height = 1;
    assert_eq!(
        Asert::from_genesis(&DEVNET, &bad),
        Err(ConsensusError::BadHeight {
            expected: 0,
            got: 1
        })
    );
    let mut bad = g.clone();
    bad.bits = 0x2100_8000; // above the devnet limit
    assert_eq!(
        Asert::from_genesis(&DEVNET, &bad),
        Err(ConsensusError::InvalidBits(0x2100_8000))
    );
    for params in [
        Params {
            target_block_time_secs: 0,
            ..DEVNET
        },
        Params {
            asert_half_life_secs: 0,
            ..DEVNET
        },
    ] {
        assert!(matches!(
            Asert::from_genesis(&params, &g),
            Err(ConsensusError::InvalidParams(_))
        ));
    }
    let params = Params {
        pow_limit_bits: 0x2080_0000,
        ..DEVNET
    };
    assert_eq!(
        Asert::from_genesis(&params, &g),
        Err(ConsensusError::InvalidPowLimit(0x2080_0000))
    );
}

proptest! {
    /// Agrees with the parameterized `asert_target`: time_diff = parent timestamp − (genesis timestamp − T),
    /// height_diff = parent height.
    #[test]
    fn next_bits_is_asert_target_anchored_at_genesis(height in any::<u64>(), timestamp in any::<u64>()) {
        let asert = devnet_asert();
        let limit = pow_limit_target(DEVNET.pow_limit_bits).unwrap();
        let time_diff = i128::from(timestamp) - i128::from(GENESIS_TIME - T);
        let target = asert_target(&limit, &limit, T, TAU, time_diff, height).unwrap();
        prop_assert_eq!(asert.next_target(height, timestamp), Ok(target));
        prop_assert_eq!(asert.next_bits(height, timestamp), Ok(bits_from_target(&target)));
        // the result is always valid bits not above the limit
        prop_assert!(target_from_bits(bits_from_target(&target), DEVNET.pow_limit_bits).is_ok());
    }

    /// Monotonicity: the later the parent timestamp the higher (easier) the target, the higher the height the lower
    /// the target; the decoded bits are monotone as well.
    #[test]
    fn monotone_in_time_and_height(
        height in 0u64..10_000_000,
        dh in 0u64..100_000,
        t in 0u64..(GENESIS_TIME + 100_000_000),
        dt in 0u64..1_000_000,
        anchor_bits in prop::sample::select(vec![0x207f_ffffu32, 0x1f00_ffff, 0x1d00_ffff, 0x0400_8000]),
    ) {
        let mut g = genesis(&DEVNET);
        g.bits = anchor_bits;
        let asert = Asert::from_genesis(&DEVNET, &g).unwrap();
        let decoded = |h: u64, ts: u64| target_from_bits(asert.next_bits(h, ts).unwrap(), DEVNET.pow_limit_bits).unwrap();
        prop_assert!(asert.next_target(height, t).unwrap() <= asert.next_target(height, t + dt).unwrap());
        prop_assert!(asert.next_target(height, t).unwrap() >= asert.next_target(height + dh, t).unwrap());
        prop_assert!(decoded(height, t) <= decoded(height, t + dt));
        prop_assert!(decoded(height, t) >= decoded(height + dh, t));
    }
}
