//! Signature checks of BTC inputs (SPEC §7.2, D-034): the producer's inputs of a settlement transaction can only be
//! P2TR key-path (BIP341) or P2WPKH (BIP143); the witness, the signature encoding (BIP340, strict DER and low S) and
//! the sighash type are checked item by item; also the BIP86 tweak of the funding key.
//!
//! The path-A funding inputs and paired outputs (D-033) were deleted along with path A (D-078); only the parts used by
//! settlement transactions remain here.

use std::borrow::Borrow;

use bitcoin::hashes::Hash;
use bitcoin::key::{CompressedPublicKey, Keypair, Secp256k1, XOnlyPublicKey};
use bitcoin::secp256k1::{self, schnorr, Message, Scalar, Signing, Verification};
use bitcoin::sighash::{EcdsaSighashType, Prevouts, SighashCache, TapSighashType};
use bitcoin::taproot::TapTweakHash;
use bitcoin::{Script, ScriptBuf, Transaction, TxOut};

/// The input's signature does not meet the requirements.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpendError {
    /// The previous output's locking script is neither P2TR nor P2WPKH.
    #[error("the previous output's locking script is neither P2TR nor P2WPKH")]
    UnsupportedInput,
    /// The number or length of the witness stack elements is wrong (including an annex, extra elements, an
    /// uncompressed public key).
    #[error("witness format error: {0}")]
    Witness(String),
    /// The sighash type does not meet the requirements (the producer's inputs must use `SIGHASH_ALL` /
    /// `SIGHASH_DEFAULT`).
    #[error("sighash type {got:#04x} does not meet the requirements (expected {expected:#04x})")]
    SighashType {
        /// The sighash type byte in the witness.
        got: u8,
        /// The required sighash type byte.
        expected: u8,
    },
    /// Signature encoding error: not a 64 / 65 byte BIP340 signature, or not strict DER (BIP66), or the S of an ECDSA
    /// signature is not low.
    #[error("signature encoding error: {0}")]
    SignatureEncoding(String),
    /// The output key of the P2TR previous output is not a valid x-only public key (the output cannot be spent by
    /// key-path).
    #[error("P2TR output key invalid")]
    InvalidOutputKey,
    /// The public key hash in the P2WPKH witness does not match the previous output's witness program.
    #[error("the public key in the witness does not match the previous output's witness program")]
    PubkeyMismatch,
    /// Signature verification failed.
    #[error("signature verification failed")]
    BadSignature,
    /// The sighash cannot be computed.
    #[error("sighash computation failed: {0}")]
    Sighash(String),
}

/// Locking scripts allowed for change outputs: P2PKH, P2SH, P2WPKH, P2WSH or P2TR (D-034).
pub fn is_standard_output_script(script: &Script) -> bool {
    script.is_p2pkh()
        || script.is_p2sh()
        || script.is_p2wpkh()
        || script.is_p2wsh()
        || script.is_p2tr()
}

/// Parses the signature element of a P2TR key-path spend: 64 bytes means `SIGHASH_DEFAULT`; with 65 bytes the last
/// byte is the sighash type (`0x00` or an undefined type is invalid, BIP341).
pub fn taproot_signature(
    element: &[u8],
) -> Result<(schnorr::Signature, TapSighashType), SpendError> {
    let (sig, sighash_type) = match element.len() {
        64 => (element, TapSighashType::Default),
        65 => {
            let (&byte, sig) = element
                .split_last()
                .ok_or_else(|| SpendError::SignatureEncoding("BIP340 signature is empty".into()))?;
            let sighash_type = match TapSighashType::from_consensus_u8(byte) {
                Ok(TapSighashType::Default) | Err(_) => {
                    return Err(SpendError::SignatureEncoding(format!(
                        "sighash type {byte:#04x} of a 65-byte signature is invalid"
                    )))
                }
                Ok(t) => t,
            };
            (sig, sighash_type)
        }
        len => {
            return Err(SpendError::SignatureEncoding(format!(
                "a BIP340 signature must be 64 or 65 bytes, got {len} bytes"
            )))
        }
    };
    let sig = schnorr::Signature::from_slice(sig)
        .map_err(|e| SpendError::SignatureEncoding(e.to_string()))?;
    Ok((sig, sighash_type))
}

/// Requires the BIP341 sighash type to be `expected`.
pub fn expect_tap_sighash(got: TapSighashType, expected: TapSighashType) -> Result<(), SpendError> {
    if got == expected {
        Ok(())
    } else {
        Err(SpendError::SighashType {
            got: tap_sighash_byte(got),
            expected: tap_sighash_byte(expected),
        })
    }
}

/// Verifies a BIP341 key-path signature: `output_script` is the P2TR output being spent (its witness program is the
/// output key).
pub fn verify_taproot_key_spend<T: Borrow<TxOut>>(
    tx: &Transaction,
    index: usize,
    prevouts: &Prevouts<'_, T>,
    output_script: &Script,
    sig: &schnorr::Signature,
    sighash_type: TapSighashType,
) -> Result<(), SpendError> {
    let key = p2tr_output_key(output_script).ok_or(SpendError::InvalidOutputKey)?;
    let sighash = SighashCache::new(tx)
        .taproot_key_spend_signature_hash(index, prevouts, sighash_type)
        .map_err(|e| SpendError::Sighash(e.to_string()))?;
    Secp256k1::verification_only()
        .verify_schnorr(sig, &Message::from(sighash), &key)
        .map_err(|_| SpendError::BadSignature)
}

/// Verifies a P2WPKH witness `[DER signature ‖ sighash type, 33-byte compressed public key]`: the public key hash
/// equals the witness program of `prevout`, the signature is strict DER (BIP66) with low S, the sighash type is
/// `sighash_type`, and it is verified against the BIP143 sighash.
pub fn verify_p2wpkh_spend(
    tx: &Transaction,
    index: usize,
    prevout: &TxOut,
    witness: &[&[u8]],
    sighash_type: EcdsaSighashType,
) -> Result<(), SpendError> {
    let (sig, public) = match witness {
        [sig, public] => (*sig, *public),
        other => {
            return Err(SpendError::Witness(format!(
                "a P2WPKH witness must have exactly 2 elements (signature, public key), got {}",
                other.len()
            )))
        }
    };
    if public.len() != 33 {
        return Err(SpendError::Witness(format!(
            "P2WPKH accepts only a 33-byte compressed public key, got {} bytes",
            public.len()
        )));
    }
    let public = CompressedPublicKey::from_slice(public)
        .map_err(|e| SpendError::Witness(format!("public key invalid: {e}")))?;
    if ScriptBuf::new_p2wpkh(&public.wpubkey_hash()) != prevout.script_pubkey {
        return Err(SpendError::PubkeyMismatch);
    }
    if !is_strict_der(sig) {
        return Err(SpendError::SignatureEncoding(
            "ECDSA signature is not strict DER encoded (BIP66)".into(),
        ));
    }
    let (&byte, der) = sig
        .split_last()
        .ok_or_else(|| SpendError::SignatureEncoding("ECDSA signature is empty".into()))?;
    let expected = ecdsa_sighash_byte(sighash_type);
    if byte != expected {
        return Err(SpendError::SighashType {
            got: byte,
            expected,
        });
    }
    let signature = secp256k1::ecdsa::Signature::from_der(der)
        .map_err(|e| SpendError::SignatureEncoding(e.to_string()))?;
    let mut normalized = signature;
    normalized.normalize_s();
    if normalized != signature {
        return Err(SpendError::SignatureEncoding(
            "the S of the ECDSA signature is not low".into(),
        ));
    }
    let sighash = SighashCache::new(tx)
        .p2wpkh_signature_hash(index, &prevout.script_pubkey, prevout.value, sighash_type)
        .map_err(|e| SpendError::Sighash(e.to_string()))?;
    Secp256k1::verification_only()
        .verify_ecdsa(&Message::from(sighash), &signature, &public.0)
        .map_err(|_| SpendError::BadSignature)
}

/// Tweaks the keypair per BIP86 (no script tree): `q = p + H_TapTweak(P)`. Same as rust-bitcoin's `TapTweak`, but
/// never panics.
pub fn bip86_keypair<C: Signing + Verification>(
    secp: &Secp256k1<C>,
    keypair: &Keypair,
) -> Result<Keypair, String> {
    let (internal, _) = keypair.x_only_public_key();
    let tweak = TapTweakHash::from_key_and_tweak(internal, None).to_byte_array();
    let scalar = Scalar::from_be_bytes(tweak).map_err(|e| e.to_string())?;
    keypair
        .add_xonly_tweak(secp, &scalar)
        .map_err(|e| e.to_string())
}

/// The output key (witness program) of a P2TR output; `None` when it is not P2TR or not a valid x-only public key.
fn p2tr_output_key(script: &Script) -> Option<XOnlyPublicKey> {
    if !script.is_p2tr() {
        return None;
    }
    script
        .as_bytes()
        .get(2..)
        .and_then(|program| XOnlyPublicKey::from_slice(program).ok())
}

/// The byte representation of an ECDSA sighash type (all standard types are below 256).
fn ecdsa_sighash_byte(t: EcdsaSighashType) -> u8 {
    u8::try_from(t.to_u32()).unwrap_or(u8::MAX)
}

/// The byte representation of a BIP341 sighash type.
fn tap_sighash_byte(t: TapSighashType) -> u8 {
    match t {
        TapSighashType::Default => 0x00,
        TapSighashType::All => 0x01,
        TapSighashType::None => 0x02,
        TapSighashType::Single => 0x03,
        TapSighashType::AllPlusAnyoneCanPay => 0x81,
        TapSighashType::NonePlusAnyoneCanPay => 0x82,
        TapSighashType::SinglePlusAnyoneCanPay => 0x83,
    }
}

/// Strict DER signature encoding (BIP66), item by item after Bitcoin Core's `IsValidSignatureEncoding`; `sig` includes
/// the trailing sighash type byte.
///
/// Format: `0x30 [total length] 0x02 [R length] [R] 0x02 [S length] [S] [sighash type]`.
pub fn is_strict_der(sig: &[u8]) -> bool {
    strict_der(sig).is_some()
}

fn strict_der(sig: &[u8]) -> Option<()> {
    let len = sig.len();
    let at = |i: usize| sig.get(i).copied();
    // lower and upper bounds on the length
    if !(9..=73).contains(&len) {
        return None;
    }
    // compound type 0x30; the length covers the whole signature (without the sighash type byte)
    if at(0)? != 0x30 || usize::from(at(1)?) != len.checked_sub(3)? {
        return None;
    }
    // length of R; the length field of S is still within the signature
    let len_r = usize::from(at(3)?);
    let s_len_at = len_r.checked_add(5)?;
    if s_len_at >= len {
        return None;
    }
    let len_s = usize::from(at(s_len_at)?);
    // the element lengths add up to the total length
    if len_r.checked_add(len_s)?.checked_add(7)? != len {
        return None;
    }
    // R: integer type, non-empty, non-negative, no superfluous leading 0
    let r0 = at(4)?;
    if at(2)? != 0x02 || len_r == 0 || r0 & 0x80 != 0 {
        return None;
    }
    if len_r > 1 && r0 == 0x00 && at(5)? & 0x80 == 0 {
        return None;
    }
    // S: as above
    let s0 = at(len_r.checked_add(6)?)?;
    if at(len_r.checked_add(4)?)? != 0x02 || len_s == 0 || s0 & 0x80 != 0 {
        return None;
    }
    if len_s > 1 && s0 == 0x00 && at(len_r.checked_add(7)?)? & 0x80 == 0 {
        return None;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::SecretKey;

    fn der(r: &[u8], s: &[u8]) -> Vec<u8> {
        let len = |n: usize| u8::try_from(n).unwrap();
        let mut sig = vec![0x30, len(4 + r.len() + s.len()), 0x02, len(r.len())];
        sig.extend_from_slice(r);
        sig.extend_from_slice(&[0x02, len(s.len())]);
        sig.extend_from_slice(s);
        sig.push(0x01);
        sig
    }

    #[test]
    fn strict_der_follows_bip66() {
        assert!(is_strict_der(&der(&[1], &[1])));
        assert!(is_strict_der(&der(&[0x00, 0x80], &[0x7f; 32])));
        assert!(is_strict_der(&der(&[0x7f; 33], &[0x7f; 32])));
        // negative, superfluous leading 0, empty integer
        assert!(!is_strict_der(&der(&[0x80], &[1])));
        assert!(!is_strict_der(&der(&[1], &[0x80])));
        assert!(!is_strict_der(&der(&[0x00, 0x01], &[1])));
        assert!(!is_strict_der(&der(&[1], &[0x00, 0x01])));
        assert!(!is_strict_der(&der(&[], &[1, 2])));
        assert!(!is_strict_der(&der(&[1, 2], &[])));
        // too long (> 73 bytes), too short
        assert!(!is_strict_der(&der(&[0x7f; 34], &[0x7f; 33])));
        assert!(!is_strict_der(&[
            0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01
        ]));
        // type byte or total length field mismatch
        let mut bad = der(&[1], &[1]);
        bad[0] = 0x31;
        assert!(!is_strict_der(&bad));
        let mut bad = der(&[1], &[1]);
        bad[1] += 1;
        assert!(!is_strict_der(&bad));
        let mut bad = der(&[1], &[1]);
        bad[2] = 0x03;
        assert!(!is_strict_der(&bad));
        // length of R out of range
        let mut bad = der(&[1], &[1]);
        bad[3] = 0x10;
        assert!(!is_strict_der(&bad));
    }

    #[test]
    fn bip86_keypair_matches_rust_bitcoin_tweak() {
        use bitcoin::key::TapTweak;

        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let keypair = Keypair::from_secret_key(&secp, &secret);
        let tweaked = bip86_keypair(&secp, &keypair).unwrap();
        let expected = keypair.tap_tweak(&secp, None).to_keypair();
        assert_eq!(tweaked, expected);
        assert_eq!(
            ScriptBuf::new_p2tr_tweaked(bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(
                tweaked.x_only_public_key().0
            )),
            crate::settlement::producer_script(&keypair.x_only_public_key().0)
        );
    }

    #[test]
    fn tap_sighash_bytes_match_consensus_values() {
        for byte in [0x00, 0x01, 0x02, 0x03, 0x81, 0x82, 0x83] {
            let t = TapSighashType::from_consensus_u8(byte).unwrap();
            assert_eq!(tap_sighash_byte(t), byte);
        }
        assert_eq!(
            ecdsa_sighash_byte(EcdsaSighashType::SinglePlusAnyoneCanPay),
            0x83
        );
    }
}
