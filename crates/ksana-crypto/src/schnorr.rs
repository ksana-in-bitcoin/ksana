//! BIP340 Schnorr signatures (32-byte message digests) and account derivation (SPEC §4, D-092).
//!
//! An account is the 32-byte output public key of the user's Taproot address (BIP86 key-path, no script tree): the
//! account of a local secret key `d` is the x-only public key of the BIP86-tweaked keypair `q = ±d + H_TapTweak(P)`,
//! and `Bip340` signatures also use this tweaked key. The address and the account are two spellings of the same bytes
//! ([`crate::account`]).
//!
//! Besides VmOp signatures, the contract host function `verify_schnorr` (SPEC §9.3) also uses the verification here.

use bitcoin::hashes::Hash;
use bitcoin::taproot::TapTweakHash;
use secp256k1::{schnorr, Message, Scalar, Secp256k1, XOnlyPublicKey};

pub use secp256k1::Keypair;

use crate::SigError;

/// Build a keypair from a 32-byte secret key (the untweaked internal key).
pub fn keypair_from_secret(secret: &[u8; 32]) -> Result<Keypair, SigError> {
    Keypair::from_seckey_slice(&Secp256k1::signing_only(), secret)
        .map_err(|_| SigError::InvalidSecretKey)
}

/// Tweak a keypair per BIP86 (no script tree): `q = p + H_TapTweak(P)`, with `P` the one with even y. Same as
/// rust-bitcoin's `TapTweak`, but never panics: returns [`SigError::InvalidSecretKey`] when the tweaked result falls
/// outside the curve order (negligible probability).
pub fn tweaked_keypair(keypair: &Keypair) -> Result<Keypair, SigError> {
    let (internal, _) = keypair.x_only_public_key();
    let tweak = TapTweakHash::from_key_and_tweak(internal, None).to_byte_array();
    let scalar = Scalar::from_be_bytes(tweak).map_err(|_| SigError::InvalidSecretKey)?;
    keypair
        .add_xonly_tweak(&Secp256k1::new(), &scalar)
        .map_err(|_| SigError::InvalidSecretKey)
}

/// The account of a keypair: the BIP86-tweaked x-only public key, i.e. the output public key of the key's Taproot
/// address (SPEC §4, D-092).
///
/// The tweak fails with negligible probability (the keypair's tweak falls outside the curve order); the all-zero
/// account is returned then: it is not the account of any key, signing with it fails with
/// [`SigError::AccountMismatch`] and never silently signs another account's op.
pub fn account_of(keypair: &Keypair) -> [u8; 32] {
    tweaked_keypair(keypair)
        .map(|kp| kp.x_only_public_key().0.serialize())
        .unwrap_or([0; 32])
}

/// BIP340-sign a 32-byte digest; `aux_rand` is BIP340's auxiliary randomness. `keypair` is the signing key itself, no
/// further tweak.
pub fn schnorr_sign(digest: &[u8; 32], keypair: &Keypair, aux_rand: &[u8; 32]) -> [u8; 64] {
    Secp256k1::signing_only()
        .sign_schnorr_with_aux_rand(&Message::from_digest(*digest), keypair, aux_rand)
        .serialize()
}

/// Verify per BIP340 that `sig` is `pubkey`'s signature over the 32-byte digest `digest`.
pub fn schnorr_verify(
    pubkey: &[u8; 32],
    digest: &[u8; 32],
    sig: &[u8; 64],
) -> Result<(), SigError> {
    let pubkey = XOnlyPublicKey::from_slice(pubkey).map_err(|_| SigError::InvalidPublicKey)?;
    let sig = schnorr::Signature::from_slice(sig).map_err(|_| SigError::InvalidSignature)?;
    Secp256k1::verification_only()
        .verify_schnorr(&sig, &Message::from_digest(*digest), &pubkey)
        .map_err(|_| SigError::InvalidSignature)
}
