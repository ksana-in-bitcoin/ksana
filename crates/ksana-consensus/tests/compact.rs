//! Compact `bits`, target and work (D-025), checked item by item against independent implementations:
//!
//! - validity per the checks of Bitcoin Core's `arith_uint256::SetCompact` and `CheckProofOfWork` (independently
//!   rewritten in this file);
//! - target value, canonical encoding and work with rust-bitcoin's `bitcoin::pow::{CompactTarget, Target, Work}`.
//!
//! rust-bitcoin serves as a reference only where it agrees with Core: `Target::from_compact` does not check overflow
//! (the shift amount is taken modulo 256), and for exponent ≤ 3 it does not treat a set sign bit as negative, so
//! target values are only compared for `bits` that Core judges valid; its `U256::inverse` defines the work of target 1
//! as 2^256 − 1 (Core's `GetBlockProof` gives 2^255), so target 1 is asserted separately when comparing work.

use bitcoin::pow::{CompactTarget, Target};
use ksana_consensus::{
    bits_from_target, pow_limit_target, target_from_bits, work_from_bits, work_from_target,
    ConsensusError, U256,
};
use proptest::prelude::*;

/// devnet's PoW limit (the same as BTC regtest, D-025).
const DEVNET_LIMIT: u32 = 0x207f_ffff;
/// BTC mainnet's minimum difficulty.
const MAINNET_LIMIT: u32 = 0x1d00_ffff;
/// The largest target compact can represent, 0xffff·2^240 = 2^256 − 2^240: every non-overflowing compact target is at
/// most this (mantissa ≤ 0x7fffff at exponent 32, ≤ 0xffff at exponent 33, ≤ 0xff at exponent 34).
const WIDE_LIMIT: u32 = 0x2100_ffff;
const LIMITS: [u32; 3] = [DEVNET_LIMIT, MAINNET_LIMIT, WIDE_LIMIT];

fn btc_target(bits: u32) -> Target {
    Target::from_compact(CompactTarget::from_consensus(bits))
}

fn to_btc(target: &U256) -> Target {
    Target::from_le_bytes(target.to_le_bytes())
}

/// Bitcoin Core's `SetCompact` + `CheckProofOfWork` validity check (independently rewritten).
fn core_accepts(bits: u32, limit_bits: u32) -> bool {
    let size = bits >> 24;
    let mut word = bits & 0x007f_ffff;
    if size <= 3 {
        word >>= 8 * (3 - size);
    }
    let negative = word != 0 && bits & 0x0080_0000 != 0;
    let overflow =
        word != 0 && (size > 34 || (word > 0xff && size > 33) || (word > 0xffff && size > 32));
    // when non-negative and non-overflowing, rust-bitcoin's decoding agrees with Core
    !negative && !overflow && word != 0 && btc_target(bits) <= btc_target(limit_bits)
}

fn arb_bits() -> impl Strategy<Value = u32> {
    prop_oneof![
        any::<u32>(),
        // exponent 0..=40, any mantissa (including the sign bit): truncation at exponent ≤ 3, overflow boundaries,
        // around the PoW limit
        (0u32..=40, 0u32..=0x00ff_ffff).prop_map(|(size, m)| (size << 24) | m),
        // very small mantissa (around the overflow boundaries 0xff / 0xffff)
        (30u32..=36, 0u32..=0x0001_0100).prop_map(|(size, m)| (size << 24) | m),
    ]
}

fn arb_target() -> impl Strategy<Value = U256> {
    (any::<[u8; 32]>(), 0u32..=256)
        .prop_map(|(bytes, keep)| U256::from_le_bytes(bytes).shr(256 - keep))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// Validity agrees with Core; when valid, the target value agrees with rust-bitcoin; when invalid, the error is
    /// `InvalidBits(bits)`.
    #[test]
    fn target_from_bits_matches_bitcoin_core(bits in arb_bits()) {
        for limit in LIMITS {
            let ours = target_from_bits(bits, limit);
            prop_assert_eq!(ours.is_ok(), core_accepts(bits, limit), "bits {:#010x} limit {:#010x}", bits, limit);
            match ours {
                Ok(target) => prop_assert_eq!(to_btc(&target), btc_target(bits)),
                Err(e) => prop_assert_eq!(e, ConsensusError::InvalidBits(bits)),
            }
        }
    }

    /// The canonical encoding agrees with rust-bitcoin's `to_compact_lossy` (i.e. Core's `GetCompact`);
    /// the decoded result is unchanged by re-encoding and decoding again, and only low bits are truncated:
    /// error < 2^(bit length − 16).
    #[test]
    fn bits_from_target_matches_get_compact(target in arb_target()) {
        let bits = bits_from_target(&target);
        prop_assert_eq!(bits, to_btc(&target).to_compact_lossy().to_consensus());
        prop_assert_eq!(bits & 0x0080_0000, 0, "sign bit must be 0");
        if target.is_zero() {
            prop_assert_eq!(bits, 0);
        } else {
            let decoded = target_from_bits(bits, WIDE_LIMIT).unwrap();
            prop_assert!(decoded <= target);
            let error = target.checked_sub(decoded).unwrap();
            prop_assert!(error.is_zero() || error.bits() + 16 <= target.bits());
            prop_assert_eq!(bits_from_target(&decoded), bits);
        }
    }

    /// Round trip of valid `bits`: decoding, canonically encoding and decoding again gives the same target; the
    /// canonical encoding is idempotent.
    #[test]
    fn canonical_round_trip(bits in arb_bits()) {
        if let Ok(target) = target_from_bits(bits, WIDE_LIMIT) {
            let canonical = bits_from_target(&target);
            prop_assert_eq!(target_from_bits(canonical, WIDE_LIMIT), Ok(target));
            prop_assert_eq!(bits_from_target(&target_from_bits(canonical, WIDE_LIMIT).unwrap()), canonical);
            prop_assert_eq!(canonical, btc_target(bits).to_compact_lossy().to_consensus());
            // the mantissa of a canonical encoding has no superfluous leading 0 byte: ≥ 0x8000 (the top byte is 0
            // only because the top bit of the next byte is 1), and for exponent < 3 the low bytes dropped by the right
            // shift are 0
            let is_canonical = |b: u32| {
                let (size, mantissa) = (b >> 24, b & 0x007f_ffff);
                let dropped = if size < 3 { mantissa & ((1 << (8 * (3 - size))) - 1) } else { 0 };
                mantissa >= 0x8000 && dropped == 0
            };
            prop_assert!(is_canonical(canonical), "{:#010x}", canonical);
            prop_assert_eq!(canonical == bits, is_canonical(bits), "{:#010x}", bits);
        }
    }

    /// The work ⌊2^256 / (target + 1)⌋ agrees with rust-bitcoin's `Target::to_work` (except for target 1, see the
    /// file header).
    #[test]
    fn work_matches_bitcoin(bits in arb_bits()) {
        if let Ok(target) = target_from_bits(bits, WIDE_LIMIT) {
            let work = work_from_bits(bits, WIDE_LIMIT).unwrap();
            prop_assert_eq!(work_from_target(&target), Ok(work));
            if target == U256::ONE {
                prop_assert_eq!(work, U256::ONE.shl(255));
            } else {
                prop_assert_eq!(work.to_le_bytes(), btc_target(bits).to_work().to_le_bytes());
            }
            // the lower the target the greater the work
            if let Some(harder) = target.checked_sub(U256::ONE).filter(|t| !t.is_zero()) {
                prop_assert!(work_from_target(&harder).unwrap() >= work);
            }
        } else {
            prop_assert_eq!(work_from_bits(bits, WIDE_LIMIT), Err(ConsensusError::InvalidBits(bits)));
        }
    }
}

/// All kinds of boundaries: sign bit, 0, truncation at exponent ≤ 3, overflow, PoW limit.
#[test]
fn edge_cases() {
    let hex = |s: &str| {
        U256::from_be_bytes(
            hex::decode(format!("{s:0>64}"))
                .unwrap()
                .try_into()
                .unwrap(),
        )
    };
    let ok = |bits: u32, limit: u32, expected: &str| {
        assert_eq!(
            target_from_bits(bits, limit),
            Ok(hex(expected)),
            "{bits:#010x}"
        );
        assert!(core_accepts(bits, limit));
    };
    let invalid = |bits: u32, limit: u32| {
        assert_eq!(
            target_from_bits(bits, limit),
            Err(ConsensusError::InvalidBits(bits)),
            "{bits:#010x}"
        );
        assert!(!core_accepts(bits, limit), "{bits:#010x}");
    };

    // 0 and the sign bit
    invalid(0, DEVNET_LIMIT);
    invalid(0x0080_0000, DEVNET_LIMIT);
    invalid(0x0480_0000, DEVNET_LIMIT); // sign bit + mantissa 0
    invalid(0x0492_3456, DEVNET_LIMIT);
    invalid(0x0380_0001, DEVNET_LIMIT);
    invalid(0x2080_0000, DEVNET_LIMIT);
    // exponent ≤ 3: the mantissa is shifted right, low bits dropped
    invalid(0x0100_3456, DEVNET_LIMIT); // 0x003456 >> 16 = 0
    ok(0x0112_3456, DEVNET_LIMIT, "12");
    ok(0x0200_8000, DEVNET_LIMIT, "80");
    ok(0x0212_3456, DEVNET_LIMIT, "1234");
    ok(0x0312_3456, DEVNET_LIMIT, "123456");
    ok(0x0101_0000, MAINNET_LIMIT, "1"); // minimum target
                                         // rust-bitcoin returns 0xfe here (not treating the sign bit as negative), Core
                                         // judges it negative: Core prevails
    invalid(0x01fe_dcba, WIDE_LIMIT);
    assert_eq!(btc_target(0x01fe_dcba), to_btc(&U256::from_u64(0xfe)));
    invalid(0x0000_0001, DEVNET_LIMIT);
    // the general case
    ok(0x0412_3456, DEVNET_LIMIT, "12345600");
    ok(0x0500_9234, DEVNET_LIMIT, "92340000");
    ok(
        MAINNET_LIMIT,
        MAINNET_LIMIT,
        "00000000ffff0000000000000000000000000000000000000000000000000000",
    );
    ok(
        DEVNET_LIMIT,
        DEVNET_LIMIT,
        "7fffff0000000000000000000000000000000000000000000000000000000000",
    );
    // above the PoW limit
    invalid(0x1d01_0000, MAINNET_LIMIT);
    invalid(0x2100_8000, DEVNET_LIMIT); // 2^255 > devnet limit
    ok(
        0x2100_8000,
        WIDE_LIMIT,
        "8000000000000000000000000000000000000000000000000000000000000000",
    );
    invalid(0x2100_ffff, DEVNET_LIMIT);
    ok(
        0x2100_ffff,
        WIDE_LIMIT,
        "ffff000000000000000000000000000000000000000000000000000000000000",
    );
    // overflow (target ≥ 2^256)
    ok(
        0x2200_00ff,
        WIDE_LIMIT,
        "ff00000000000000000000000000000000000000000000000000000000000000",
    );
    invalid(0x2200_0100, WIDE_LIMIT); // 2^256
    invalid(0x2101_0000, WIDE_LIMIT); // 2^256
    invalid(0x2300_0001, WIDE_LIMIT);
    invalid(0xff00_0001, WIDE_LIMIT);
    invalid(0xff7f_ffff, WIDE_LIMIT);
    invalid(0xff00_0000, WIDE_LIMIT); // mantissa 0
    invalid(u32::MAX, WIDE_LIMIT);

    // non-canonical encodings decode too (bit-for-bit identity is guaranteed by the block header check)
    assert_eq!(
        target_from_bits(0x0500_0100, DEVNET_LIMIT),
        target_from_bits(0x0401_0000, DEVNET_LIMIT)
    );
    assert_eq!(bits_from_target(&hex("1000000")), 0x0401_0000);
    // non-zero dropped low bytes at exponent ≤ 3 are also a non-canonical encoding: 0x01010001 decodes to 1, the
    // canonical encoding is 0x01010000
    ok(0x0101_0001, DEVNET_LIMIT, "1");
    assert_eq!(bits_from_target(&U256::ONE), 0x0101_0000);
    ok(0x0212_34ff, DEVNET_LIMIT, "1234");
    assert_eq!(bits_from_target(&hex("1234")), 0x0212_3400);
}

#[test]
fn invalid_pow_limit_is_a_parameter_error() {
    for limit in [0, 0x0080_0000, 0x2080_0000, 0x2200_0100, 0xff00_0001] {
        assert_eq!(
            pow_limit_target(limit),
            Err(ConsensusError::InvalidPowLimit(limit))
        );
        assert_eq!(
            target_from_bits(0x1d00_ffff, limit),
            Err(ConsensusError::InvalidPowLimit(limit))
        );
    }
    assert_eq!(
        pow_limit_target(DEVNET_LIMIT).unwrap().to_hex(),
        "7fffff0000000000000000000000000000000000000000000000000000000000"
    );
}

#[test]
fn bits_from_target_edge_cases() {
    assert_eq!(bits_from_target(&U256::ZERO), 0);
    assert_eq!(bits_from_target(&U256::ONE), 0x0101_0000);
    assert_eq!(bits_from_target(&U256::from_u64(0x7f)), 0x017f_0000);
    // the exponent is incremented when the mantissa's top bit is 1
    assert_eq!(bits_from_target(&U256::from_u64(0x80)), 0x0200_8000);
    assert_eq!(bits_from_target(&U256::from_u64(0x0012_3456)), 0x0312_3456);
    assert_eq!(bits_from_target(&U256::from_u64(0x0092_3456)), 0x0400_9234);
    assert_eq!(bits_from_target(&U256::MAX), 0x2100_ffff);
    assert_eq!(
        bits_from_target(&pow_limit_target(DEVNET_LIMIT).unwrap()),
        DEVNET_LIMIT
    );
    // 2^255 − 2^232 + 1: low bits truncated
    let limit_plus = pow_limit_target(DEVNET_LIMIT)
        .unwrap()
        .checked_add(U256::ONE)
        .unwrap();
    assert_eq!(bits_from_target(&limit_plus), DEVNET_LIMIT);
    for target in [U256::ONE, U256::MAX, limit_plus, U256::ONE.shl(200)] {
        assert_eq!(
            bits_from_target(&target),
            to_btc(&target).to_compact_lossy().to_consensus()
        );
    }
}

/// Known work values: the chainwork of the Bitcoin mainnet genesis block 0x100010001, that of the regtest genesis
/// block 2, and 2^255 for target 1.
#[test]
fn work_known_values() {
    assert_eq!(
        work_from_bits(MAINNET_LIMIT, MAINNET_LIMIT),
        Ok(U256::from_u64(0x1_0001_0001))
    );
    assert_eq!(
        work_from_bits(DEVNET_LIMIT, DEVNET_LIMIT),
        Ok(U256::from_u64(2))
    );
    assert_eq!(
        work_from_bits(0x0101_0000, DEVNET_LIMIT),
        Ok(U256::ONE.shl(255))
    );
    // 2^256 / 2^255 = 2, 2^256 / (2^256 − 2^248 + 1) = 1
    assert_eq!(
        work_from_target(&U256::ONE.shl(255).checked_sub(U256::ONE).unwrap()),
        Ok(U256::from_u64(2))
    );
    assert_eq!(work_from_bits(WIDE_LIMIT, WIDE_LIMIT), Ok(U256::ONE));
    assert_eq!(work_from_target(&U256::MAX), Ok(U256::ONE));
    // the work of target 0, 2^256, is not representable (valid bits never decode to 0)
    assert_eq!(work_from_target(&U256::ZERO), Err(ConsensusError::Overflow));
    assert_eq!(
        work_from_bits(0x207f_ffff, MAINNET_LIMIT),
        Err(ConsensusError::InvalidBits(0x207f_ffff))
    );
}
