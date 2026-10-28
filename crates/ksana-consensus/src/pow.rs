//! The numeric conventions of the PoW (D-025, all inherited from Bitcoin): compact `bits`, target, PoW value and work;
//! the effective target of the two-speed difficulty (D-084).

use ksana_primitives::BlockHeader;

use crate::{ConsensusError, PowAlgorithm, U256};

/// The sign bit of the compact encoding.
const SIGN_BIT: u32 = 0x0080_0000;
/// The mantissa of the compact encoding (without the sign bit).
const MANTISSA_MASK: u32 = 0x007f_ffff;

/// Compact decoding (the semantics of Bitcoin's `SetCompact`): target = mantissa × 256^(exponent − 3); when the
/// exponent is ≤ 3 the mantissa is shifted right and the low bits are dropped.
///
/// Returns `None` when the sign bit is set, the target is 0 or the target is ≥ 2^256. Equivalent to Bitcoin Core's
/// `fNegative || fOverflow || target == 0` check: Core only treats it as negative when the mantissa is non-zero, but a
/// zero mantissa gives target 0, which is invalid as well.
fn decode_compact(bits: u32) -> Option<U256> {
    if bits & SIGN_BIT != 0 {
        return None;
    }
    let size = bits >> 24;
    let mantissa = U256::from_u64(u64::from(bits & MANTISSA_MASK));
    let target = match size.checked_sub(3) {
        // exponent 0..=2: the mantissa is shifted right by 8·(3 − exponent) bits
        None => mantissa.shr(3u32.abs_diff(size).saturating_mul(8)),
        Some(exponent) => {
            // exponent ≤ 255, shift amount ≤ 2016, does not saturate
            let shift = exponent.saturating_mul(8);
            if mantissa.bits().saturating_add(shift) > U256::BITS {
                // a zero mantissa has bit length 0 and never gets here; a non-zero mantissa shifted left is ≥ 2^256
                return None;
            }
            mantissa.shl(shift)
        }
    };
    (!target.is_zero()).then_some(target)
}

/// The target value of the PoW limit (minimum difficulty); `pow_limit_bits` itself must be a valid compact target.
pub fn pow_limit_target(pow_limit_bits: u32) -> Result<U256, ConsensusError> {
    decode_compact(pow_limit_bits).ok_or(ConsensusError::InvalidPowLimit(pow_limit_bits))
}

/// Compact `bits` → target (D-025).
///
/// Sign bit (`0x0080_0000`) set, target 0, target ≥ 2^256 and target above the PoW limit all return
/// [`ConsensusError::InvalidBits`]; returns [`ConsensusError::InvalidPowLimit`] when `pow_limit_bits` itself is
/// invalid. `bits` is not required to be canonically encoded — a block's `bits` must be bit-for-bit identical to the
/// canonical value computed by the difficulty adjustment, which the block header check guarantees.
pub fn target_from_bits(bits: u32, pow_limit_bits: u32) -> Result<U256, ConsensusError> {
    let limit = pow_limit_target(pow_limit_bits)?;
    match decode_compact(bits) {
        Some(target) if target <= limit => Ok(target),
        _ => Err(ConsensusError::InvalidBits(bits)),
    }
}

/// Target → canonical compact encoding (Bitcoin's `GetCompact`): the highest 3 significant bytes become the mantissa
/// (low bits truncated); when the mantissa's top bit is 1 it is shifted right by one byte and the exponent incremented,
/// so the sign bit is 0. Target 0 encodes as 0.
///
/// For any target decoded from valid `bits`, the result decodes back to exactly the original target.
pub fn bits_from_target(target: &U256) -> u32 {
    let mut size = target.bits().div_ceil(8); // number of significant bytes, 0..=32
    let window = match size.checked_sub(3) {
        None => target.shl(3u32.abs_diff(size).saturating_mul(8)),
        Some(extra) => target.shr(extra.saturating_mul(8)),
    };
    let [m0, m1, m2, ..] = window.low_u64().to_le_bytes();
    let mut mantissa = u32::from_le_bytes([m0, m1, m2, 0]);
    if mantissa & SIGN_BIT != 0 {
        mantissa >>= 8;
        size = size.saturating_add(1);
    }
    mantissa | (size << 24)
}

/// The work corresponding to a target: ⌊2^256 / (target + 1)⌋ (D-025), i.e. the expected number of attempts needed to
/// find a hash not greater than the target.
///
/// Computed by the identity of Bitcoin Core's `GetBlockProof`: ⌊(2^256 − 1 − target) / (target + 1)⌋ + 1.
/// For target 0 the result 2^256 is not representable and [`ConsensusError::Overflow`] is returned (valid `bits` never
/// decode to 0).
pub fn work_from_target(target: &U256) -> Result<U256, ConsensusError> {
    let Some(divisor) = target.checked_add(U256::ONE) else {
        // target = 2^256 − 1: 2^256 / 2^256 = 1
        return Ok(U256::ONE);
    };
    let complement = U256::MAX
        .checked_sub(*target)
        .ok_or(ConsensusError::Overflow)?;
    complement
        .checked_div(divisor)
        .and_then(|quotient| quotient.checked_add(U256::ONE))
        .ok_or(ConsensusError::Overflow)
}

/// The work of valid `bits`, ⌊2^256 / (target + 1)⌋; returns the same error as [`target_from_bits`] when `bits` is
/// invalid.
pub fn work_from_bits(bits: u32, pow_limit_bits: u32) -> Result<U256, ConsensusError> {
    work_from_target(&target_from_bits(bits, pow_limit_bits)?)
}

/// The PoW value of a 32-byte hash: interpreted as **little-endian** (the byte-order convention of D-025).
/// [`crate::Argon2id`] follows this convention for its 32-byte output, and the test-only [`crate::Sha256d`] for the
/// block hash (the raw output of `sha256d`, i.e. Bitcoin's internal byte order).
///
/// For a block hash this is equivalent to treating the hex shown by RPC / block explorers (the reverse of the internal
/// byte order) as a big-endian number.
pub fn pow_value(hash: &[u8; 32]) -> U256 {
    U256::from_le_bytes(*hash)
}

/// The effective target of a block header with weight `weight` (two-speed difficulty, D-084): the target of `bits`
/// divided by the weight (integer division). For weight 1 (a non-empty block) it is just the target of `bits`; an
/// empty block's weight doubles with the streak from `EMPTY_BLOCK_DIFFICULTY` up to `MAX_EMPTY_BLOCK_WEIGHT` (D-090).
/// Returns [`ConsensusError::TargetTooSmall`] when the result is 0 (target too small to be met) or `weight` is 0;
/// returns the same error as [`target_from_bits`] when `bits` is invalid.
pub fn effective_target(
    bits: u32,
    pow_limit_bits: u32,
    weight: u64,
) -> Result<U256, ConsensusError> {
    let target = target_from_bits(bits, pow_limit_bits)?;
    let too_small = || ConsensusError::TargetTooSmall { bits, weight };
    let effective = target
        .checked_div(U256::from_u64(weight))
        .ok_or_else(too_small)?;
    if effective.is_zero() {
        return Err(too_small());
    }
    Ok(effective)
}

/// PoW check: `bits` is valid and `algorithm.pow_value(header) ≤ effective target` (D-062; the effective target is by
/// weight, D-084). Does not check whether `bits` matches the difficulty adjustment. Returns
/// [`ConsensusError::PowUnavailable`] when the algorithm's computation fails (a local fault, not an invalid block).
pub fn check_pow(
    algorithm: &dyn PowAlgorithm,
    header: &BlockHeader,
    pow_limit_bits: u32,
    weight: u64,
) -> Result<(), ConsensusError> {
    let target = effective_target(header.bits, pow_limit_bits, weight)?;
    if algorithm.pow_value(header)? <= target {
        Ok(())
    } else {
        Err(ConsensusError::InsufficientPow)
    }
}
