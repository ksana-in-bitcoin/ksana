//! Integer ASERT (aserti3-2d) difficulty adjustment anchored at the genesis block (D-026).
//!
//! The formula (all integer arithmetic):
//!
//! ```text
//! exponent = trunc((time_diff − T·(height_diff + 1)) · 65536 / τ)      truncated toward zero
//! shifts   = floor(exponent / 65536)
//! frac     = exponent − shifts · 65536                                   ∈ [0, 65535]
//! factor   = 65536 + ((195766423245049·frac + 971821376·frac² + 5127·frac³ + 2^47) >> 48)
//! next     = floor(anchor · factor · 2^(shifts − 16)), 1 when it is 0, the PoW limit when it exceeds the limit
//! ```
//!
//! The reference implementation (Bitcoin Cash Node's `CalculateASERT`) requires the PoW limit < 2^224 and
//! `|time_diff − T·height_diff| < 2^47`; here the computation uses exact wide integers, the result equals the formula
//! above at arbitrary precision, and under the reference implementation's preconditions it is bit-for-bit identical to
//! the reference implementation (`tests/asert_vectors.rs` checks against the official test vectors).

use ksana_primitives::{BlockHeader, Params};

use crate::pow::{bits_from_target, pow_limit_target, target_from_bits};
use crate::u256::{limbs_bits, limbs_mul_u64, limbs_shl, limbs_shr};
use crate::{ConsensusError, U256};

/// Number of fixed-point fraction bits: the exponent is in units of 1/65536.
const RADIX_BITS: u32 = 16;
/// Fixed-point coefficients of the cubic polynomial 2^x ≈ 1 + 0.695502049·x + 0.2262698·x² + 0.0782318·x³ (as in the
/// reference implementation).
const POLY_1: u64 = 195_766_423_245_049;
const POLY_2: u64 = 971_821_376;
const POLY_3: u64 = 5_127;
const POLY_ROUND: u64 = 1 << 47;

/// At |exponent| ≥ 2^24 (i.e. |shifts| ≥ 256) the result is already saturated:
/// for shifts ≥ 256, `anchor·factor·2^(shifts−16) ≥ 2^16·2^240 = 2^256`, greater than any PoW limit, so the limit
/// is taken; for shifts ≤ −256, `anchor·factor < 2^273`, shifted right by at least 272 bits it is ≤ 1, so the result
/// is 1. Clamping the exponent to ±2^40 therefore does not change the result, and the remaining arithmetic can be done
/// in i64 / u32.
const EXPONENT_CLAMP: u128 = 1 << 40;

/// Computes the child block's target by the aserti3-2d formula (D-026). Parameterized so it can be checked against the
/// public test vectors.
///
/// - `anchor_target`: the anchor block's target, must be within `[1, pow_limit]`;
/// - `spacing_secs`: the target block interval T; `half_life_secs`: the half-life τ (must be greater than 0);
/// - `time_diff`: parent timestamp − anchor parent time (may be negative);
/// - `height_diff`: parent height − anchor height.
///
/// No input overflows: intermediate values are computed with exact wide integers, and out-of-range results saturate
/// to 1 or the PoW limit per the formula.
pub fn asert_target(
    anchor_target: &U256,
    pow_limit: &U256,
    spacing_secs: u64,
    half_life_secs: u64,
    time_diff: i128,
    height_diff: u64,
) -> Result<U256, ConsensusError> {
    if half_life_secs == 0 {
        return Err(ConsensusError::InvalidParams(
            "ASERT half-life must be greater than 0",
        ));
    }
    if anchor_target.is_zero() || anchor_target > pow_limit {
        return Err(ConsensusError::InvalidParams(
            "ASERT anchor target must be within [1, PoW limit]",
        ));
    }
    let exponent = exponent(spacing_secs, half_life_secs, time_diff, height_diff)?;
    // arithmetic right shift = floor; the low 16 bits (two's complement) = exponent − shifts·65536
    let shifts = exponent >> RADIX_BITS;
    let frac = u64::try_from(exponent & 0xffff).map_err(|_| ConsensusError::Overflow)?;
    let factor = factor(frac).ok_or(ConsensusError::Overflow)?;

    // anchor·factor < 2^256·2^17, represented exactly with 5 limbs (320 bits)
    let (low, carry) = limbs_mul_u64(&anchor_target.to_limbs(), factor);
    let [l0, l1, l2, l3] = low;
    let product = [l0, l1, l2, l3, carry];

    let shift = shifts
        .checked_sub(i64::from(RADIX_BITS))
        .ok_or(ConsensusError::Overflow)?;
    let next = if shift >= 0 {
        let shift = u32::try_from(shift).map_err(|_| ConsensusError::Overflow)?;
        if limbs_bits(&product).saturating_add(shift) > U256::BITS {
            // result ≥ 2^256, necessarily above the PoW limit
            return Ok(*pow_limit);
        }
        narrow(limbs_shl(&product, shift))
    } else {
        // |shift| ≤ 2^24 + 16; a larger right shift also gives 0
        let shift = u32::try_from(shift.unsigned_abs()).unwrap_or(u32::MAX);
        narrow(limbs_shr(&product, shift))
    };
    Ok(match next {
        None => *pow_limit, // ≥ 2^256
        Some(target) if target.is_zero() => U256::ONE,
        Some(target) => target.min(*pow_limit),
    })
}

/// The fixed-point exponent `trunc((time_diff − T·(height_diff + 1))·65536 / τ)`, clamped to ±[`EXPONENT_CLAMP`].
fn exponent(
    spacing_secs: u64,
    half_life_secs: u64,
    time_diff: i128,
    height_diff: u64,
) -> Result<i64, ConsensusError> {
    // T·(height_diff + 1) ≤ (2^64 − 1)·2^64 < 2^128, does not overflow u128
    let ideal = u128::from(height_diff)
        .checked_add(1)
        .and_then(|blocks| blocks.checked_mul(u128::from(spacing_secs)))
        .ok_or(ConsensusError::Overflow)?;
    // sign and absolute value of the numerator time_diff − ideal; None when the absolute value is ≥ 2^128 (necessarily
    // saturated)
    let (negative, magnitude) = if time_diff >= 0 {
        let t = time_diff.unsigned_abs();
        (t < ideal, Some(t.abs_diff(ideal)))
    } else {
        (true, time_diff.unsigned_abs().checked_add(ideal))
    };
    // truncation toward zero: |exponent| = floor(|numerator|·65536 / τ), then apply the sign
    let abs = magnitude
        .and_then(|m| m.checked_mul(1 << RADIX_BITS))
        .and_then(|m| m.checked_div(u128::from(half_life_secs)))
        .map_or(EXPONENT_CLAMP, |q| q.min(EXPONENT_CLAMP));
    let abs = i64::try_from(abs).map_err(|_| ConsensusError::Overflow)?;
    if negative {
        abs.checked_neg().ok_or(ConsensusError::Overflow)
    } else {
        Ok(abs)
    }
}

/// The cubic polynomial approximation of `65536·2^(frac/65536)`, `frac ∈ [0, 65535]`; the result is within
/// [65536, 131071]. At frac = 65535 the polynomial is about 2^64 − 1.8·10^14, so u64 does not overflow (consistent
/// with the reference implementation).
fn factor(frac: u64) -> Option<u64> {
    let frac2 = frac.checked_mul(frac)?;
    let frac3 = frac2.checked_mul(frac)?;
    let poly = POLY_1
        .checked_mul(frac)?
        .checked_add(POLY_2.checked_mul(frac2)?)?
        .checked_add(POLY_3.checked_mul(frac3)?)?
        .checked_add(POLY_ROUND)?;
    (1u64 << RADIX_BITS).checked_add(poly >> 48)
}

/// 320 bits → 256 bits; returns `None` when the highest limb is non-zero (≥ 2^256).
fn narrow(limbs: [u64; 5]) -> Option<U256> {
    let [l0, l1, l2, l3, l4] = limbs;
    (l4 == 0).then_some(U256::from_limbs([l0, l1, l2, l3]))
}

/// ASERT anchored at the genesis block (D-026): anchor height 0, anchor target = the target of the genesis block's
/// `bits`, anchor parent time = genesis timestamp − `TARGET_BLOCK_TIME` (the exponent is 0 when blocks arrive exactly
/// on schedule).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asert {
    anchor_target: U256,
    anchor_parent_time: i128,
    spacing_secs: u64,
    half_life_secs: u64,
    pow_limit: U256,
}

impl Asert {
    /// Anchored at `genesis`. The genesis block height must be 0 and its `bits` must be a valid target not above the
    /// PoW limit; `TARGET_BLOCK_TIME` and `ASERT_HALF_LIFE` must be greater than 0.
    pub fn from_genesis(params: &Params, genesis: &BlockHeader) -> Result<Self, ConsensusError> {
        if params.target_block_time_secs == 0 {
            return Err(ConsensusError::InvalidParams(
                "TARGET_BLOCK_TIME must be greater than 0",
            ));
        }
        if params.asert_half_life_secs == 0 {
            return Err(ConsensusError::InvalidParams(
                "ASERT_HALF_LIFE must be greater than 0",
            ));
        }
        if genesis.height != 0 {
            return Err(ConsensusError::BadHeight {
                expected: 0,
                got: genesis.height,
            });
        }
        let pow_limit = pow_limit_target(params.pow_limit_bits)?;
        let anchor_target = target_from_bits(genesis.bits, params.pow_limit_bits)?;
        let anchor_parent_time = i128::from(genesis.timestamp)
            .checked_sub(i128::from(params.target_block_time_secs))
            .ok_or(ConsensusError::Overflow)?;
        Ok(Self {
            anchor_target,
            anchor_parent_time,
            spacing_secs: params.target_block_time_secs,
            half_life_secs: params.asert_half_life_secs,
            pow_limit,
        })
    }

    /// The child block's target when the parent has height `parent_height` and timestamp `parent_timestamp`.
    pub fn next_target(
        &self,
        parent_height: u64,
        parent_timestamp: u64,
    ) -> Result<U256, ConsensusError> {
        let time_diff = i128::from(parent_timestamp)
            .checked_sub(self.anchor_parent_time)
            .ok_or(ConsensusError::Overflow)?;
        // the anchor height is 0: height_diff = parent_height
        asert_target(
            &self.anchor_target,
            &self.pow_limit,
            self.spacing_secs,
            self.half_life_secs,
            time_diff,
            parent_height,
        )
    }

    /// The `bits` (canonical encoding) the child block must carry when the parent has height `parent_height` and
    /// timestamp `parent_timestamp`.
    pub fn next_bits(
        &self,
        parent_height: u64,
        parent_timestamp: u64,
    ) -> Result<u32, ConsensusError> {
        Ok(bits_from_target(
            &self.next_target(parent_height, parent_timestamp)?,
        ))
    }

    /// The anchor target (the target of the genesis block's `bits`).
    pub fn anchor_target(&self) -> U256 {
        self.anchor_target
    }

    /// The anchor parent time: genesis timestamp − `TARGET_BLOCK_TIME` (may be negative).
    pub fn anchor_parent_time(&self) -> i128 {
        self.anchor_parent_time
    }

    /// The target of the PoW limit.
    pub fn pow_limit(&self) -> U256 {
        self.pow_limit
    }
}
