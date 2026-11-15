//! Half-aggregation of BIP340 signatures (SPEC §4.5, D-154): `u` signatures `(r_i, s_i)` become the `u` values `r_i`
//! and one scalar `s = Σ z_i·s_i`, as the draft specification `half-aggregation.mediawiki` of
//! BlockstreamResearch/cross-input-aggregation has it (Aggregate and VerifyAggregate, at commit `e44d778`).
//!
//! - `z_0 = 1` and `z_i = int(hash_{HalfAgg/randomizer}(r_0 ‖ pk_0 ‖ m_0 ‖ … ‖ r_i ‖ pk_i ‖ m_i)) mod n` for `i > 0`;
//! - the aggregate verifies when every `pk_i` and every `r_i` is the x coordinate of a point (`P_i`, `R_i`, with even
//!   y), `s < n`, and `s·G = Σ z_i·R_i + Σ (z_i·e_i)·P_i` with BIP340's challenge
//!   `e_i = int(hash_{BIP0340/challenge}(r_i ‖ pk_i ‖ m_i)) mod n`.
//!
//! An aggregate that verifies does not prove that each `(r_i, s_i)` it was made from verifies alone (the draft's
//! `test_aggregate_verify_strange`); it proves that each key's holder signed its message. A consensus rule must
//! therefore be stated on the aggregate, never on the signatures it was made from.
//!
//! Only libsecp256k1's public interface is used (D-154): every term is one multiplication of a point (the terms of
//! one public key are added first), the scalars are secret keys with zero kept apart, and the sum is taken once.
//! Consensus code: deterministic, no panics, errors are always returned as [`SigError`].

use std::collections::BTreeMap;

use secp256k1::constants::{CURVE_ORDER, ONE};
use secp256k1::{Parity, PublicKey, Scalar, Secp256k1, SecretKey, XOnlyPublicKey};
use sha2::{Digest, Sha256};

use crate::SigError;

/// The most signatures one aggregate holds (the draft specification's bound, `u < 2^16`).
pub const MAX_AGGREGATED: usize = 0xffff;

const RANDOMIZER_TAG: &str = "HalfAgg/randomizer";
const CHALLENGE_TAG: &str = "BIP0340/challenge";

/// What stays of one signature in an aggregate, and what it is checked against: the signer's x-only public key, the
/// 32-byte message and the first half `r` of the BIP340 signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HalfSigned {
    /// The x-only public key.
    pub pubkey: [u8; 32],
    /// The message that was signed (a 32-byte digest).
    pub msg: [u8; 32],
    /// The first 32 bytes of the BIP340 signature.
    pub r: [u8; 32],
}

/// A scalar modulo the curve order `n`: libsecp256k1's secret key, which is never zero, or `None` for zero.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Sc(Option<SecretKey>);

impl Sc {
    const ZERO: Sc = Sc(None);

    /// `int(bytes)` when it is below `n`.
    fn strict(bytes: &[u8; 32]) -> Option<Sc> {
        if bytes == &[0; 32] {
            return Some(Sc::ZERO);
        }
        SecretKey::from_slice(bytes).ok().map(|key| Sc(Some(key)))
    }

    /// `int(bytes) mod n`: `2^256 < 2n`, so `n` is taken off at most once.
    fn reduce(bytes: &[u8; 32]) -> Sc {
        Sc::strict(bytes)
            .or_else(|| Sc::strict(&minus_order(bytes)))
            .unwrap_or(Sc::ZERO)
    }

    fn tweak(self) -> Option<Scalar> {
        self.0.map(Scalar::from)
    }

    /// `self · other mod n`. The product of two non-zero scalars is not zero, so the library's refusal is passed on
    /// as an error rather than read as zero.
    fn mul(self, other: Sc) -> Result<Sc, SigError> {
        match (self.0, other.tweak()) {
            (Some(key), Some(tweak)) => key
                .mul_tweak(&tweak)
                .map(|key| Sc(Some(key)))
                .map_err(|_| SigError::InvalidSignature),
            _ => Ok(Sc::ZERO),
        }
    }

    /// `self + other mod n`: the library refuses a sum only when it is zero.
    fn add(self, other: Sc) -> Sc {
        match (self.0, other.tweak()) {
            (Some(key), Some(tweak)) => Sc(key.add_tweak(&tweak).ok()),
            (None, _) => other,
            (_, None) => self,
        }
    }

    fn to_bytes(self) -> [u8; 32] {
        self.0.map_or([0; 32], |key| key.secret_bytes())
    }
}

/// `int(bytes) - n mod 2^256`, both big-endian.
fn minus_order(bytes: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut borrow = false;
    for ((out, byte), order) in out.iter_mut().zip(bytes).zip(&CURVE_ORDER).rev() {
        let (diff, under) = byte.overflowing_sub(*order);
        let (diff, under_borrow) = diff.overflowing_sub(u8::from(borrow));
        *out = diff;
        borrow = under || under_borrow;
    }
    out
}

fn tagged(tag: &str) -> Sha256 {
    let tag_hash = Sha256::digest(tag.as_bytes());
    let mut hasher = Sha256::new();
    hasher.update(tag_hash);
    hasher.update(tag_hash);
    hasher
}

/// The randomizers `z_i` in order: the hash runs over all the signatures so far and is read after each.
struct Randomizers {
    hasher: Sha256,
    first: bool,
}

impl Randomizers {
    fn new() -> Self {
        Self {
            hasher: tagged(RANDOMIZER_TAG),
            first: true,
        }
    }

    fn next(&mut self, item: &HalfSigned) -> Sc {
        self.hasher.update(item.r);
        self.hasher.update(item.pubkey);
        self.hasher.update(item.msg);
        if std::mem::take(&mut self.first) {
            return Sc::reduce(&ONE);
        }
        Sc::reduce(&self.hasher.clone().finalize().into())
    }
}

/// BIP340's challenge `e` of one signature.
fn challenge(item: &HalfSigned) -> Sc {
    let mut hasher = tagged(CHALLENGE_TAG);
    hasher.update(item.r);
    hasher.update(item.pubkey);
    hasher.update(item.msg);
    Sc::reduce(&hasher.finalize().into())
}

/// The point with even y whose x coordinate is `x`, if there is one.
fn lift_x(x: &[u8; 32]) -> Option<PublicKey> {
    XOnlyPublicKey::from_slice(x)
        .ok()
        .map(|key| key.public_key(Parity::Even))
}

fn check_len(len: usize) -> Result<(), SigError> {
    if len > MAX_AGGREGATED {
        return Err(SigError::AggregateTooLarge);
    }
    Ok(())
}

/// Aggregate: the scalar `s = Σ z_i·s_i` of the signatures `(items[i].r, s_halves[i])`, in the order given. The
/// signatures are not verified here; an `s_i` that is not below `n` (no BIP340 signature has one) is refused.
pub fn aggregate(items: &[HalfSigned], s_halves: &[[u8; 32]]) -> Result<[u8; 32], SigError> {
    check_len(items.len())?;
    if items.len() != s_halves.len() {
        return Err(SigError::InvalidSignature);
    }
    let mut randomizers = Randomizers::new();
    let mut sum = Sc::ZERO;
    for (item, s) in items.iter().zip(s_halves) {
        let s = Sc::strict(s).ok_or(SigError::InvalidSignature)?;
        sum = sum.add(randomizers.next(item).mul(s)?);
    }
    Ok(sum.to_bytes())
}

/// VerifyAggregate: whether `agg_s` is the aggregate of signatures by `items[i].pubkey` over `items[i].msg` whose
/// first halves are `items[i].r`, in this order. No signatures and a zero `agg_s` verify, as in the draft.
pub fn verify_aggregate(items: &[HalfSigned], agg_s: &[u8; 32]) -> Result<(), SigError> {
    check_len(items.len())?;
    let secp = Secp256k1::verification_only();
    let mut terms: Vec<PublicKey> = Vec::with_capacity(items.len().saturating_mul(2));
    let mut by_key: BTreeMap<[u8; 32], Sc> = BTreeMap::new();
    let mut randomizers = Randomizers::new();
    for item in items {
        let r_point = lift_x(&item.r).ok_or(SigError::InvalidSignature)?;
        let z = randomizers.next(item);
        if let Some(z) = z.tweak() {
            terms.push(
                r_point
                    .mul_tweak(&secp, &z)
                    .map_err(|_| SigError::InvalidSignature)?,
            );
        }
        let term = z.mul(challenge(item))?;
        let sum = by_key.entry(item.pubkey).or_insert(Sc::ZERO);
        *sum = sum.add(term);
    }
    for (pubkey, scalar) in &by_key {
        let point = lift_x(pubkey).ok_or(SigError::InvalidPublicKey)?;
        if let Some(scalar) = scalar.tweak() {
            terms.push(
                point
                    .mul_tweak(&secp, &scalar)
                    .map_err(|_| SigError::InvalidSignature)?,
            );
        }
    }
    let s = Sc::strict(agg_s).ok_or(SigError::InvalidSignature)?;
    // The library refuses a sum only when it is the point at infinity, which is what `None` stands for on both sides.
    let sum = match terms.iter().collect::<Vec<_>>().as_slice() {
        [] => None,
        terms => PublicKey::combine_keys(terms).ok(),
    };
    let expected =
        s.0.map(|key| PublicKey::from_secret_key(&Secp256k1::signing_only(), &key));
    if sum == expected {
        Ok(())
    } else {
        Err(SigError::InvalidSignature)
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::schnorr::{keypair_from_secret, schnorr_sign, schnorr_verify};

    fn bytes32(hex: &str) -> [u8; 32] {
        hex::decode(hex).unwrap().try_into().unwrap()
    }

    /// The signature the draft's tests make from the byte `i`: secret key `[i + 1; 32]`, message `[i + 2; 32]`,
    /// auxiliary randomness `[i + 3; 32]`.
    fn draft_signature(i: u8) -> (HalfSigned, [u8; 64]) {
        signature(&[i + 1; 32], [i + 2; 32], &[i + 3; 32])
    }

    fn signature(secret: &[u8; 32], msg: [u8; 32], aux: &[u8; 32]) -> (HalfSigned, [u8; 64]) {
        let keypair = keypair_from_secret(secret).unwrap();
        let sig = schnorr_sign(&msg, &keypair, aux);
        let item = HalfSigned {
            pubkey: keypair.x_only_public_key().0.serialize(),
            msg,
            r: sig[..32].try_into().unwrap(),
        };
        (item, sig)
    }

    fn s_half(sig: &[u8; 64]) -> [u8; 32] {
        sig[32..].try_into().unwrap()
    }

    fn aggregate_of(signed: &[(HalfSigned, [u8; 64])]) -> (Vec<HalfSigned>, [u8; 32]) {
        let items: Vec<HalfSigned> = signed.iter().map(|(item, _)| *item).collect();
        let halves: Vec<[u8; 32]> = signed.iter().map(|(_, sig)| s_half(sig)).collect();
        let s = aggregate(&items, &halves).unwrap();
        (items, s)
    }

    /// `test_verify_vectors` of the draft (hacspec-halfagg/tests/tests.rs): public keys, messages and the aggregate
    /// `r_0 ‖ … ‖ s`.
    const DRAFT_VECTORS: [(&[(&str, &str)], &str); 3] = [
        (&[], "0000000000000000000000000000000000000000000000000000000000000000"),
        (
            &[(
                "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f",
                "0202020202020202020202020202020202020202020202020202020202020202",
            )],
            "b070aafcea439a4f6f1bbfc2eb66d29d24b0cab74d6b745c3cfb009cc8fe4aa80e066c34819936549ff49b6fd4d41edfc401a367b87ddd59fee38177961c225f",
        ),
        (
            &[
                (
                    "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f",
                    "0202020202020202020202020202020202020202020202020202020202020202",
                ),
                (
                    "462779ad4aad39514614751a71085f2f10e1c7a593e4e030efb5b8721ce55b0b",
                    "0505050505050505050505050505050505050505050505050505050505050505",
                ),
            ],
            "b070aafcea439a4f6f1bbfc2eb66d29d24b0cab74d6b745c3cfb009cc8fe4aa8a3afbdb45a6a34bf7c8c00f1b6d7e7d375b54540f13716c87b62e51e2f4f22ffbf8913ec53226a34892d60252a7052614ca79ae939986828d81d2311957371ad",
        ),
    ];

    fn draft_vector(index: usize) -> (Vec<HalfSigned>, [u8; 32]) {
        let (keys, aggsig) = DRAFT_VECTORS[index];
        let aggsig = hex::decode(aggsig).unwrap();
        let items = keys
            .iter()
            .zip(aggsig.chunks(32))
            .map(|((pubkey, msg), r)| HalfSigned {
                pubkey: bytes32(pubkey),
                msg: bytes32(msg),
                r: r.try_into().unwrap(),
            })
            .collect();
        (items, aggsig[aggsig.len() - 32..].try_into().unwrap())
    }

    #[test]
    fn the_drafts_vectors_verify() {
        for index in 0..DRAFT_VECTORS.len() {
            let (items, s) = draft_vector(index);
            assert_eq!(verify_aggregate(&items, &s), Ok(()), "vector {index}");
        }
    }

    /// The draft made its vectors from secret key `[1; 32]`, message `[2; 32]`, auxiliary randomness `[3; 32]` and
    /// `[4; 32]`, `[5; 32]`, `[6; 32]`: aggregating the same signatures gives the same bytes.
    #[test]
    fn aggregating_the_drafts_signatures_gives_its_vectors() {
        let signed = [
            signature(&[1; 32], [2; 32], &[3; 32]),
            signature(&[4; 32], [5; 32], &[6; 32]),
        ];
        for count in 0..=signed.len() {
            let (items, s) = aggregate_of(&signed[..count]);
            assert_eq!((items, s), draft_vector(count), "{count} signatures");
        }
    }

    #[test]
    fn aggregates_of_one_to_many_signatures_verify_and_any_change_fails() {
        for count in [1u8, 2, 3, 17] {
            let signed: Vec<_> = (0..count).map(draft_signature).collect();
            let count = usize::from(count);
            let (items, s) = aggregate_of(&signed);
            assert_eq!(verify_aggregate(&items, &s), Ok(()), "{count} signatures");

            let mut wrong_s = s;
            wrong_s[31] ^= 1;
            assert_eq!(
                verify_aggregate(&items, &wrong_s),
                Err(SigError::InvalidSignature)
            );
            for index in 0..count {
                let mut changed = items.clone();
                changed[index].msg[0] ^= 1;
                assert_eq!(
                    verify_aggregate(&changed, &s),
                    Err(SigError::InvalidSignature),
                    "message {index} of {count}"
                );
                let mut changed = items.clone();
                changed[index].pubkey = draft_signature(200).0.pubkey;
                assert_eq!(
                    verify_aggregate(&changed, &s),
                    Err(SigError::InvalidSignature),
                    "key {index} of {count}"
                );
                let mut changed = items.clone();
                changed[index].r = draft_signature(201).0.r;
                assert_eq!(
                    verify_aggregate(&changed, &s),
                    Err(SigError::InvalidSignature),
                    "r {index} of {count}"
                );
            }
            if count > 1 {
                let mut swapped = items.clone();
                swapped.swap(0, 1);
                assert_eq!(
                    verify_aggregate(&swapped, &s),
                    Err(SigError::InvalidSignature),
                    "order of {count}"
                );
                assert_eq!(
                    verify_aggregate(&items[..count - 1], &s),
                    Err(SigError::InvalidSignature),
                    "one fewer of {count}"
                );
            }
        }
    }

    /// One key signing several messages: its terms are added before the multiplication.
    #[test]
    fn signatures_of_one_key_verify_together_with_others() {
        let signed: Vec<_> = (0..12u8)
            .map(|i| signature(&[1 + i % 3; 32], [100 + i; 32], &[i; 32]))
            .collect();
        let (items, s) = aggregate_of(&signed);
        assert_eq!(verify_aggregate(&items, &s), Ok(()));
        let mut changed = items.clone();
        changed[7].msg[5] ^= 1;
        assert_eq!(
            verify_aggregate(&changed, &s),
            Err(SigError::InvalidSignature)
        );
    }

    /// The draft's `test_aggregate_verify_strange`: two signatures that do not verify alone and whose aggregate
    /// verifies, which is why a rule must be stated on the aggregate.
    #[test]
    fn an_aggregate_can_verify_while_its_signatures_do_not() {
        let signed = [draft_signature(0), draft_signature(1)];
        let (items, s) = aggregate_of(&signed);

        let mut randomizers = Randomizers::new();
        let _ = randomizers.next(&items[0]);
        let z1 = randomizers.next(&items[1]);
        // s0' = s - z1·s1' for an arbitrary s1', since z0 = 1
        let s1 = Sc::reduce(&[0x5a; 32]);
        let minus = Sc(z1.mul(s1).unwrap().0.map(SecretKey::negate));
        let s0 = Sc::strict(&s).unwrap().add(minus);

        let mut sig0 = signed[0].1;
        sig0[32..].copy_from_slice(&s0.to_bytes());
        let mut sig1 = signed[1].1;
        sig1[32..].copy_from_slice(&s1.to_bytes());
        for (item, sig) in [(&items[0], &sig0), (&items[1], &sig1)] {
            assert_eq!(
                schnorr_verify(&item.pubkey, &item.msg, sig),
                Err(SigError::InvalidSignature)
            );
        }
        let strange = aggregate(&items, &[s_half(&sig0), s_half(&sig1)]).unwrap();
        assert_eq!(strange, s);
        assert_eq!(verify_aggregate(&items, &strange), Ok(()));
    }

    #[test]
    fn edge_cases() {
        // no signatures: only the zero scalar
        assert_eq!(aggregate(&[], &[]), Ok([0; 32]));
        assert_eq!(verify_aggregate(&[], &[0; 32]), Ok(()));
        assert_eq!(verify_aggregate(&[], &ONE), Err(SigError::InvalidSignature));

        let (item, sig) = draft_signature(0);
        let s = aggregate(&[item], &[s_half(&sig)]).unwrap();
        // one signature's aggregate is the signature
        assert_eq!(s, s_half(&sig));

        // s not below the order
        assert_eq!(
            verify_aggregate(&[item], &CURVE_ORDER),
            Err(SigError::InvalidSignature)
        );
        assert_eq!(
            verify_aggregate(&[], &CURVE_ORDER),
            Err(SigError::InvalidSignature)
        );
        assert_eq!(
            aggregate(&[item], &[CURVE_ORDER]),
            Err(SigError::InvalidSignature)
        );
        // a zero s with signatures
        assert_eq!(
            verify_aggregate(&[item], &[0; 32]),
            Err(SigError::InvalidSignature)
        );

        // x coordinates that are no point's: 5 has no y on the curve, and the field's prime is out of range
        let not_on_curve = {
            let mut x = [0u8; 32];
            x[31] = 5;
            x
        };
        let field_prime =
            bytes32("fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f");
        for x in [not_on_curve, field_prime] {
            assert_eq!(
                verify_aggregate(&[HalfSigned { pubkey: x, ..item }], &s),
                Err(SigError::InvalidPublicKey)
            );
            assert_eq!(
                verify_aggregate(&[HalfSigned { r: x, ..item }], &s),
                Err(SigError::InvalidSignature)
            );
        }

        // lengths
        assert_eq!(aggregate(&[item], &[]), Err(SigError::InvalidSignature));
        let many = vec![item; MAX_AGGREGATED + 1];
        assert_eq!(
            verify_aggregate(&many, &s),
            Err(SigError::AggregateTooLarge)
        );
        assert_eq!(
            aggregate(&many, &vec![[1; 32]; many.len()]),
            Err(SigError::AggregateTooLarge)
        );
    }

    #[test]
    fn scalars_reduce_and_add_modulo_the_order() {
        assert!(Sc::reduce(&[0; 32]) == Sc::ZERO);
        assert!(Sc::reduce(&CURVE_ORDER) == Sc::ZERO);
        let mut order_plus_one = CURVE_ORDER;
        order_plus_one[31] += 1;
        assert!(Sc::reduce(&order_plus_one) == Sc::reduce(&ONE));
        // 2^256 - 1 mod n, computed independently (Python: (2**256 - 1) % n)
        assert_eq!(
            Sc::reduce(&[0xff; 32]).to_bytes(),
            bytes32("000000000000000000000000000000014551231950b75fc4402da1732fc9bebe")
        );
        assert!(Sc::strict(&CURVE_ORDER).is_none());
        assert!(Sc::strict(&[0xff; 32]).is_none());

        let one = Sc::reduce(&ONE);
        let mut order_minus_one = CURVE_ORDER;
        order_minus_one[31] -= 1;
        let minus_one = Sc::strict(&order_minus_one).unwrap();
        assert!(one.add(minus_one) == Sc::ZERO);
        assert!(minus_one.mul(minus_one).unwrap() == one);
        assert!(Sc::ZERO.mul(one).unwrap() == Sc::ZERO);
        assert!(Sc::ZERO.add(one) == one);
        assert!(one.add(Sc::ZERO) == one);
    }
}
