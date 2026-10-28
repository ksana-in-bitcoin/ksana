//! 256-bit unsigned integer: PoW target, PoW value and work (D-025), and the wide-integer computation of ASERT (D-026).
//!
//! Only the operations consensus needs are implemented. Operations that may overflow are all `checked_*`; shifts drop
//! the bits shifted out. The internal limb helpers are generic over little-endian limb arrays of any length; ASERT
//! uses them for its 320-bit intermediate computation.

use core::cmp::Ordering;
use core::fmt;

/// 256-bit unsigned integer made of 4 `u64` limbs in little-endian order (limb 0 is the lowest 64 bits).
///
/// Comparison ([`Ord`]) is by numeric value. [`fmt::Display`] and [`U256::to_hex`] print 64 lowercase hex digits
/// (big-endian, the same format Bitcoin Core uses to show targets), for logs.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Default, borsh::BorshSerialize, borsh::BorshDeserialize,
)]
pub struct U256([u64; 4]);

impl U256 {
    /// 0.
    pub const ZERO: Self = Self([0; 4]);
    /// 1.
    pub const ONE: Self = Self([1, 0, 0, 0]);
    /// 2^256 − 1.
    pub const MAX: Self = Self([u64::MAX; 4]);
    /// Bit width.
    pub const BITS: u32 = 256;

    /// Constructs from a `u64`.
    pub const fn from_u64(value: u64) -> Self {
        Self([value, 0, 0, 0])
    }

    /// Constructs from a `u128`.
    pub const fn from_u128(value: u128) -> Self {
        let (lo, hi) = split_u128(value);
        Self([lo, hi, 0, 0])
    }

    /// Constructs from little-endian limbs (`limbs[0]` is the lowest 64 bits).
    pub const fn from_limbs(limbs: [u64; 4]) -> Self {
        Self(limbs)
    }

    /// The little-endian limbs (`[0]` is the lowest 64 bits).
    pub const fn to_limbs(self) -> [u64; 4] {
        self.0
    }

    /// The lowest 64 bits.
    pub const fn low_u64(&self) -> u64 {
        self.0[0]
    }

    /// 32 bytes interpreted as little-endian.
    pub fn from_le_bytes(bytes: [u8; 32]) -> Self {
        let mut limbs = [0u64; 4];
        for (limb, chunk) in limbs.iter_mut().zip(bytes.as_chunks::<8>().0) {
            *limb = u64::from_le_bytes(*chunk);
        }
        Self(limbs)
    }

    /// The 32 little-endian bytes.
    pub fn to_le_bytes(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (chunk, limb) in out.as_chunks_mut::<8>().0.iter_mut().zip(self.0) {
            *chunk = limb.to_le_bytes();
        }
        out
    }

    /// 32 bytes interpreted as big-endian.
    pub fn from_be_bytes(mut bytes: [u8; 32]) -> Self {
        bytes.reverse();
        Self::from_le_bytes(bytes)
    }

    /// The 32 big-endian bytes.
    pub fn to_be_bytes(self) -> [u8; 32] {
        let mut bytes = self.to_le_bytes();
        bytes.reverse();
        bytes
    }

    /// Whether it is 0.
    pub const fn is_zero(&self) -> bool {
        let [a, b, c, d] = self.0;
        a == 0 && b == 0 && c == 0 && d == 0
    }

    /// Bit length: the minimum number of binary digits needed to represent the number (0 has bit length 0).
    pub fn bits(&self) -> u32 {
        limbs_bits(&self.0)
    }

    /// Addition, `None` on overflow.
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        let mut out = [0u64; 4];
        let mut carry = false;
        for ((dst, a), b) in out.iter_mut().zip(self.0).zip(rhs.0) {
            let (sum, c1) = a.overflowing_add(b);
            let (sum, c2) = sum.overflowing_add(u64::from(carry));
            *dst = sum;
            carry = c1 || c2;
        }
        (!carry).then_some(Self(out))
    }

    /// Subtraction, `None` when the result would be negative.
    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        let (diff, borrow) = self.overflowing_sub(rhs);
        (!borrow).then_some(diff)
    }

    /// Multiplication by a `u64`, `None` on overflow.
    pub fn checked_mul_u64(self, rhs: u64) -> Option<Self> {
        let (low, carry) = limbs_mul_u64(&self.0, rhs);
        (carry == 0).then_some(Self(low))
    }

    /// Floor division, `None` when the divisor is 0.
    pub fn checked_div(self, rhs: Self) -> Option<Self> {
        self.div_rem(rhs).map(|(quotient, _)| quotient)
    }

    /// Quotient and remainder (floor), `None` when the divisor is 0.
    pub fn div_rem(self, divisor: Self) -> Option<(Self, Self)> {
        if divisor.is_zero() {
            return None;
        }
        if self < divisor {
            return Some((Self::ZERO, self));
        }
        // restoring division: shift the divisor left to the same bit length as the dividend and trial-subtract bit by
        // bit, shift + 1 steps in all. Invariant: when handling bit i, rem < d·2, so each step subtracts at most once;
        // the left shift loses no bits (bit length ≤ 256).
        let shift = self.bits().saturating_sub(divisor.bits());
        let mut d = divisor.shl(shift);
        let mut rem = self;
        let mut quotient = Self::ZERO;
        for _ in 0..=shift {
            quotient = quotient.shl(1);
            if rem >= d {
                rem = rem.overflowing_sub(d).0;
                quotient.0[0] |= 1;
            }
            d = d.shr(1);
        }
        Some((quotient, rem))
    }

    /// Shift left by `n` bits. **Bits shifted out past the top are dropped** (no overflow reported); the result is 0
    /// for `n ≥ 256`.
    #[allow(clippy::should_implement_trait)]
    pub fn shl(self, n: u32) -> Self {
        Self(limbs_shl(&self.0, n))
    }

    /// Shift right by `n` bits (floor), bits shifted out past the bottom are dropped; the result is 0 for `n ≥ 256`.
    #[allow(clippy::should_implement_trait)]
    pub fn shr(self, n: u32) -> Self {
        Self(limbs_shr(&self.0, n))
    }

    /// 64 lowercase hex digits (big-endian, no `0x` prefix).
    pub fn to_hex(&self) -> String {
        self.to_string()
    }

    /// Subtraction, returns (the wrapped difference, whether it borrowed).
    fn overflowing_sub(self, rhs: Self) -> (Self, bool) {
        let mut out = [0u64; 4];
        let mut borrow = false;
        for ((dst, a), b) in out.iter_mut().zip(self.0).zip(rhs.0) {
            let (diff, b1) = a.overflowing_sub(b);
            let (diff, b2) = diff.overflowing_sub(u64::from(borrow));
            *dst = diff;
            borrow = b1 || b2;
        }
        (Self(out), borrow)
    }
}

impl From<u64> for U256 {
    fn from(value: u64) -> Self {
        Self::from_u64(value)
    }
}

impl From<u128> for U256 {
    fn from(value: u128) -> Self {
        Self::from_u128(value)
    }
}

impl Ord for U256 {
    fn cmp(&self, other: &Self) -> Ordering {
        // compare starting from the highest limb
        self.0.iter().rev().cmp(other.0.iter().rev())
    }
}

impl PartialOrd for U256 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for U256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.to_be_bytes() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for U256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "U256(0x{self})")
    }
}

/// Splits a `u128` into (low 64 bits, high 64 bits).
const fn split_u128(value: u128) -> (u64, u64) {
    let [a0, a1, a2, a3, a4, a5, a6, a7, b0, b1, b2, b3, b4, b5, b6, b7] = value.to_le_bytes();
    (
        u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
        u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]),
    )
}

/// The bit length of a little-endian limb array (0 has bit length 0).
pub(crate) fn limbs_bits<const N: usize>(limbs: &[u64; N]) -> u32 {
    let mut bits = 0u32;
    let mut base = 0u32;
    for limb in limbs {
        if *limb != 0 {
            bits = base.saturating_add(u64::BITS.saturating_sub(limb.leading_zeros()));
        }
        base = base.saturating_add(u64::BITS);
    }
    bits
}

/// Shifts a little-endian limb array left by `n` bits, bits shifted out past the top are dropped; the result is 0 for
/// `n ≥ 64·N`.
pub(crate) fn limbs_shl<const N: usize>(limbs: &[u64; N], n: u32) -> [u64; N] {
    let mut out = [0u64; N];
    let limb_shift = usize::try_from(n / 64).unwrap_or(usize::MAX);
    let bit_shift = n % 64;
    for (i, dst) in out.iter_mut().enumerate() {
        let Some(src) = i.checked_sub(limb_shift) else {
            continue;
        };
        let current = limbs.get(src).copied().unwrap_or(0);
        let below = src
            .checked_sub(1)
            .and_then(|j| limbs.get(j))
            .copied()
            .unwrap_or(0);
        // bit_shift < 64; when bit_shift == 0, checked_shr(64) is None and the lower limb contributes nothing
        *dst = (current << bit_shift)
            | below
                .checked_shr(64u32.wrapping_sub(bit_shift))
                .unwrap_or(0);
    }
    out
}

/// Shifts a little-endian limb array right by `n` bits (floor); the result is 0 for `n ≥ 64·N`.
pub(crate) fn limbs_shr<const N: usize>(limbs: &[u64; N], n: u32) -> [u64; N] {
    let mut out = [0u64; N];
    let limb_shift = usize::try_from(n / 64).unwrap_or(usize::MAX);
    let bit_shift = n % 64;
    for (i, dst) in out.iter_mut().enumerate() {
        let Some(src) = i.checked_add(limb_shift) else {
            continue;
        };
        let current = limbs.get(src).copied().unwrap_or(0);
        let above = src
            .checked_add(1)
            .and_then(|j| limbs.get(j))
            .copied()
            .unwrap_or(0);
        // bit_shift < 64; when bit_shift == 0, checked_shl(64) is None and the upper limb contributes nothing
        *dst = (current >> bit_shift)
            | above
                .checked_shl(64u32.wrapping_sub(bit_shift))
                .unwrap_or(0);
    }
    out
}

/// Multiplies a little-endian limb array by `m`, returning (the low 64·N bits, the topmost carry limb) — i.e. the
/// exact 64·(N+1)-bit product.
pub(crate) fn limbs_mul_u64<const N: usize>(limbs: &[u64; N], m: u64) -> ([u64; N], u64) {
    let mut out = [0u64; N];
    let mut carry = 0u64;
    for (dst, &a) in out.iter_mut().zip(limbs) {
        // a·m + carry ≤ (2^64 − 1)² + (2^64 − 1) < 2^128, does not wrap
        let wide = u128::from(a)
            .wrapping_mul(u128::from(m))
            .wrapping_add(u128::from(carry));
        let (lo, hi) = split_u128(wide);
        *dst = lo;
        carry = hi;
    }
    (out, carry)
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test code: deliberately truncates to u64 limbs
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn u(v: u128) -> U256 {
        U256::from_u128(v)
    }

    fn arb_u256() -> impl Strategy<Value = U256> {
        // random bit length, covering small numbers and numbers close to 2^256
        (any::<[u8; 32]>(), 0u32..=256)
            .prop_map(|(bytes, keep)| U256::from_le_bytes(bytes).shr(256 - keep))
    }

    /// Test-only 256×256 → 512-bit multiplication (little-endian limbs), an independent check of the division.
    fn mul_wide(a: U256, b: U256) -> [u64; 8] {
        let mut out = [0u128; 9];
        for (i, &x) in a.0.iter().enumerate() {
            for (j, &y) in b.0.iter().enumerate() {
                let p = u128::from(x) * u128::from(y);
                out[i + j] += p & u128::from(u64::MAX);
                out[i + j + 1] += p >> 64;
            }
        }
        let mut limbs = [0u64; 8];
        let mut carry = 0u128;
        for k in 0..8 {
            let v = out[k] + carry;
            limbs[k] = v as u64;
            carry = v >> 64;
        }
        assert_eq!(carry + out[8], 0);
        limbs
    }

    /// Clears the top `n` bits one by one (an independent test implementation).
    fn clear_top_bits(x: U256, n: u32) -> U256 {
        let mut be = x.to_be_bytes();
        for (i, byte) in be.iter_mut().enumerate() {
            for bit in 0..8u32 {
                if i as u32 * 8 + bit < n {
                    *byte &= !(0x80u8 >> bit);
                }
            }
        }
        U256::from_be_bytes(be)
    }

    /// Clears the low `n` bits one by one (an independent test implementation).
    fn clear_low_bits(x: U256, n: u32) -> U256 {
        let mut le = x.to_le_bytes();
        for (i, byte) in le.iter_mut().enumerate() {
            for bit in 0..8u32 {
                if i as u32 * 8 + bit < n {
                    *byte &= !(1u8 << bit);
                }
            }
        }
        U256::from_le_bytes(le)
    }

    fn widen(x: U256) -> [u64; 8] {
        let mut limbs = [0u64; 8];
        limbs[..4].copy_from_slice(&x.0);
        limbs
    }

    fn add_wide(a: [u64; 8], b: [u64; 8]) -> [u64; 8] {
        let mut out = [0u64; 8];
        let mut carry = 0u128;
        for k in 0..8 {
            let v = u128::from(a[k]) + u128::from(b[k]) + carry;
            out[k] = v as u64;
            carry = v >> 64;
        }
        assert_eq!(carry, 0);
        out
    }

    #[test]
    fn constants() {
        assert!(U256::ZERO.is_zero());
        assert_eq!(U256::ONE, U256::from_u64(1));
        assert_eq!(U256::MAX.to_be_bytes(), [0xff; 32]);
        assert_eq!(U256::MAX.bits(), 256);
        assert_eq!(U256::ZERO.bits(), 0);
        assert_eq!(U256::ONE.bits(), 1);
        assert_eq!(U256::default(), U256::ZERO);
        assert_eq!(U256::ONE.shl(255).bits(), 256);
        assert_eq!(U256::from(7u64), U256::from_u64(7));
        assert_eq!(
            U256::from(u128::MAX),
            U256::from_limbs([u64::MAX, u64::MAX, 0, 0])
        );
        assert_eq!(U256::from_limbs([1, 2, 3, 4]).to_limbs(), [1, 2, 3, 4]);
    }

    #[test]
    fn byte_order_and_hex() {
        let mut le = [0u8; 32];
        le[0] = 0x01; // lowest byte
        le[31] = 0xab; // highest byte
        let x = U256::from_le_bytes(le);
        assert_eq!(x.low_u64(), 1);
        assert_eq!(x.to_limbs()[3], 0xab << 56);
        assert_eq!(x.to_be_bytes()[0], 0xab);
        assert_eq!(
            x.to_hex(),
            "ab00000000000000000000000000000000000000000000000000000000000001"
        );
        assert_eq!(format!("{x}"), x.to_hex());
        assert_eq!(format!("{:?}", U256::ONE), format!("U256(0x{:064x})", 1));
    }

    #[test]
    fn shifts_edge_cases() {
        assert_eq!(U256::ONE.shl(0), U256::ONE);
        assert_eq!(U256::ONE.shl(64), U256::from_limbs([0, 1, 0, 0]));
        assert_eq!(U256::ONE.shl(255).shr(255), U256::ONE);
        assert_eq!(U256::ONE.shl(256), U256::ZERO);
        assert_eq!(U256::MAX.shl(u32::MAX), U256::ZERO);
        assert_eq!(U256::MAX.shr(256), U256::ZERO);
        assert_eq!(U256::MAX.shr(u32::MAX), U256::ZERO);
        // bits shifted out past the top are dropped
        assert_eq!(U256::MAX.shl(1), U256::MAX.checked_sub(U256::ONE).unwrap());
        assert_eq!(U256::MAX.shl(200).shr(200), U256::MAX.shr(200));
        assert_eq!(U256::MAX.shr(193), U256::from_u64((1 << 63) - 1));
    }

    #[test]
    fn arithmetic_edge_cases() {
        assert_eq!(U256::MAX.checked_add(U256::ONE), None);
        assert_eq!(U256::MAX.checked_add(U256::ZERO), Some(U256::MAX));
        assert_eq!(U256::ZERO.checked_sub(U256::ONE), None);
        assert_eq!(
            U256::ONE.shl(128).checked_sub(U256::ONE),
            Some(U256::from_u128(u128::MAX))
        );
        assert_eq!(U256::MAX.checked_mul_u64(2), None);
        assert_eq!(U256::MAX.checked_mul_u64(1), Some(U256::MAX));
        assert_eq!(U256::MAX.checked_mul_u64(0), Some(U256::ZERO));
        assert_eq!(U256::ONE.checked_div(U256::ZERO), None);
        assert_eq!(U256::MAX.checked_div(U256::MAX), Some(U256::ONE));
        assert_eq!(U256::MAX.checked_div(U256::ONE), Some(U256::MAX));
        assert_eq!(
            U256::MAX.div_rem(U256::ONE.shl(255)),
            Some((U256::ONE, U256::MAX.shr(1)))
        );
        // 2^256 / 2 − 1 = 2^255 − 1 remainder 1
        assert_eq!(
            U256::MAX.div_rem(U256::from_u64(2)),
            Some((U256::MAX.shr(1), U256::ONE))
        );
        assert_eq!(
            U256::from_u64(5).div_rem(U256::from_u64(7)),
            Some((U256::ZERO, U256::from_u64(5)))
        );
    }

    proptest! {
        #[test]
        fn bytes_round_trip(bytes in any::<[u8; 32]>()) {
            let x = U256::from_le_bytes(bytes);
            prop_assert_eq!(x.to_le_bytes(), bytes);
            let mut be = bytes;
            be.reverse();
            prop_assert_eq!(x.to_be_bytes(), be);
            prop_assert_eq!(U256::from_be_bytes(be), x);
            prop_assert_eq!(x.to_hex(), hex::encode(be));
        }

        #[test]
        fn order_matches_big_endian_bytes(a in arb_u256(), b in arb_u256()) {
            prop_assert_eq!(a.cmp(&b), a.to_be_bytes().cmp(&b.to_be_bytes()));
            prop_assert_eq!(a == b, a.to_be_bytes() == b.to_be_bytes());
        }

        #[test]
        fn matches_u128(a in any::<u128>(), b in any::<u128>(), m in any::<u64>(), n in 0u32..300) {
            let (x, y) = (u(a), u(b));
            prop_assert_eq!(x.cmp(&y), a.cmp(&b));
            prop_assert_eq!(x.bits(), 128 - a.leading_zeros());
            // the sum of two u128 is below 2^129: the low 128 bits plus the carry
            let (low, carry) = a.overflowing_add(b);
            let expected_sum = U256::from_limbs([low as u64, (low >> 64) as u64, u64::from(carry), 0]);
            prop_assert_eq!(x.checked_add(y), Some(expected_sum));
            prop_assert_eq!(x.checked_sub(y), a.checked_sub(b).map(u));
            if let Some(p) = a.checked_mul(u128::from(m)) {
                prop_assert_eq!(x.checked_mul_u64(m), Some(u(p)));
            }
            prop_assert_eq!(x.checked_div(y), a.checked_div(b).map(u));
            prop_assert_eq!(x.div_rem(y), a.checked_div(b).map(|q| (u(q), u(a % b))));
            prop_assert_eq!(x.shr(n), if n < 128 { u(a >> n) } else { U256::ZERO });
            if n < 128 && a.leading_zeros() >= n {
                prop_assert_eq!(x.shl(n), u(a << n));
            }
        }

        #[test]
        fn shifts_lose_only_shifted_out_bits(x in arb_u256(), n in 0u32..=300) {
            // a left shift drops the top n bits, a right shift drops the low n bits
            prop_assert_eq!(x.shl(n).shr(n), clear_top_bits(x, n));
            prop_assert_eq!(x.shr(n).shl(n), clear_low_bits(x, n));
            prop_assert_eq!(x.shr(n).bits(), x.bits().saturating_sub(n));
            if n < 256 {
                // right shift = division by 2^n; a left shift that loses no bits = multiplication by 2^n (checked
                // with the independent 512-bit multiplication)
                prop_assert_eq!(Some(x.shr(n)), x.checked_div(U256::ONE.shl(n)));
                if x.bits() + n <= 256 {
                    prop_assert_eq!(widen(x.shl(n)), mul_wide(x, U256::ONE.shl(n)));
                }
            }
        }

        #[test]
        fn mul_and_add_match_wide_arithmetic(x in arb_u256(), m in any::<u64>(), y in arb_u256()) {
            let product = mul_wide(x, U256::from_u64(m));
            let fits = product[4..].iter().all(|&l| l == 0);
            prop_assert_eq!(x.checked_mul_u64(m).is_some(), fits);
            if let Some(p) = x.checked_mul_u64(m) {
                prop_assert_eq!(widen(p), product);
            }
            let sum = add_wide(widen(x), widen(y));
            prop_assert_eq!(x.checked_add(y).is_some(), sum[4] == 0);
            if let Some(s) = x.checked_add(y) {
                prop_assert_eq!(widen(s), sum);
                prop_assert_eq!(s.checked_sub(y), Some(x));
            }
        }

        #[test]
        fn division_identity(n in arb_u256(), d in arb_u256()) {
            prop_assume!(!d.is_zero());
            let (q, r) = n.div_rem(d).unwrap();
            prop_assert!(r < d);
            // n = q·d + r, checked with the independent 512-bit multiplication
            prop_assert_eq!(add_wide(mul_wide(q, d), widen(r)), widen(n));
        }
    }
}
