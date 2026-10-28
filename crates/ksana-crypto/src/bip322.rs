//! BIP322 simple signatures (SPEC §4.2, DECISIONS D-052): used by browser wallets to sign VmOps.
//!
//! - **Message**: the lowercase hex string of the VmOp signature hash ([`crate::vmop_sighash`]) (64 ASCII bytes,
//!   [`vmop_message`]); BIP322's `message_hash = tagged_hash("BIP0322-signed-message", message)`.
//! - **Signature bytes**: the Bitcoin consensus encoding of the witness stack of the `to_sign` input (the
//!   base64-decoded bytes returned by the wallet), at most [`MAX_BIP322_SIG_BYTES`] and fully consumed.
//! - **Address** (`message_challenge`): the account's Taproot address `OP_1 <account>` (D-092: the account is the
//!   output public key, no further tweak), the witness has exactly 1 item, a P2TR key-path signature (64 bytes, or
//!   65 bytes with `SIGHASH_ALL` as the last byte). Every other form (including the two-item P2WPKH witness once
//!   accepted by D-052) fails verification. [`verify_simple`] itself remains a generic BIP322 simple verification
//!   (P2TR key-path and P2WPKH) for checking the official vectors; the consensus path only builds P2TR addresses
//!   through [`verify_for_account`].
//! - `to_spend` / `to_sign` are built strictly per BIP322 ([`to_spend`], [`to_sign`]).
//!
//! Consensus code: deterministic, no panics, errors are always returned as [`SigError`].

use bitcoin::absolute::LockTime;
use bitcoin::consensus::encode::{deserialize, serialize};
use bitcoin::hashes::Hash;
use bitcoin::key::{
    CompressedPublicKey, Keypair, Secp256k1, TapTweak, TweakedPublicKey, XOnlyPublicKey,
};
use bitcoin::opcodes::all::{OP_PUSHBYTES_0, OP_RETURN};
use bitcoin::script::{Builder, Script, ScriptBuf};
use bitcoin::secp256k1::{ecdsa, schnorr, Message};
use bitcoin::sighash::{EcdsaSighashType, Prevouts, SighashCache, TapSighashType};
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

use crate::tagged::tagged_hash;
use crate::SigError;

/// The tagged hash tag of the BIP322 message hash.
pub const BIP322_TAG: &str = "BIP0322-signed-message";

/// Length limit of `Bip322Simple` signature bytes (D-052): a P2TR witness is at most 67 bytes, with ample margin
/// (consensus parameter, unchanged by D-092).
pub const MAX_BIP322_SIG_BYTES: usize = 1_024;

/// `SIGHASH_ALL`.
const SIGHASH_ALL: u8 = 0x01;

/// BIP322's `message_hash`.
pub fn message_hash(message: &[u8]) -> [u8; 32] {
    tagged_hash(BIP322_TAG, message)
}

/// The BIP322 message of a VmOp: the bytes of the lowercase hex string of the signature hash (D-052).
pub fn vmop_message(sighash: &[u8; 32]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(64);
    for byte in sighash {
        out.extend(
            [byte >> 4, byte & 0x0f]
                .iter()
                .filter_map(|n| HEX.get(usize::from(*n)).copied()),
        );
    }
    out
}

/// BIP322's `to_spend` transaction: version 0, `nLockTime` 0; the only input spends `000…000:0xFFFFFFFF`, with
/// scriptSig `OP_0 PUSH32[message_hash]` and `nSequence` 0; the only output has amount 0 and locking script
/// `message_challenge`.
pub fn to_spend(message_challenge: &Script, message: &[u8]) -> Transaction {
    let script_sig = Builder::new()
        .push_opcode(OP_PUSHBYTES_0)
        .push_slice(message_hash(message))
        .into_script();
    Transaction {
        version: Version(0),
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::all_zeros(),
                vout: 0xFFFF_FFFF,
            },
            script_sig,
            sequence: Sequence(0),
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: message_challenge.to_owned(),
        }],
    }
}

/// BIP322's `to_sign` transaction (simple format): version 0, `nLockTime` 0; the only input spends `to_spend:0`, with
/// `nSequence` 0 and the signature as witness; the only output has amount 0 and locking script `OP_RETURN`.
pub fn to_sign(to_spend_txid: Txid, witness: Witness) -> Transaction {
    Transaction {
        version: Version(0),
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: to_spend_txid,
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence(0),
            witness,
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: Builder::new().push_opcode(OP_RETURN).into_script(),
        }],
    }
}

/// Decode a simple signature: the consensus encoding of the witness stack; the length limit is checked first and the
/// input must be fully consumed.
fn decode_witness(signature: &[u8]) -> Result<Witness, SigError> {
    if signature.len() > MAX_BIP322_SIG_BYTES {
        return Err(SigError::InvalidSignature);
    }
    deserialize::<Witness>(signature).map_err(|_| SigError::InvalidSignature)
}

/// Verify a BIP322 simple signature over `message`, where `message_challenge` is the locking script of the signing
/// address.
///
/// Only P2TR key-path and P2WPKH are supported (D-052); any other locking script returns
/// [`SigError::InvalidSignature`].
pub fn verify_simple(
    message_challenge: &Script,
    message: &[u8],
    signature: &[u8],
) -> Result<(), SigError> {
    let witness = decode_witness(signature)?;
    let spend = to_spend(message_challenge, message);
    let prevout = TxOut {
        value: Amount::ZERO,
        script_pubkey: message_challenge.to_owned(),
    };
    let tx = to_sign(spend.compute_txid(), witness);
    let witness = tx
        .input
        .first()
        .map(|i| i.witness.to_vec())
        .ok_or(SigError::InvalidSignature)?;
    let secp = Secp256k1::verification_only();
    if message_challenge.is_p2tr() {
        let [element] = witness.as_slice() else {
            return Err(SigError::InvalidSignature);
        };
        let (sig, hash_type) = match element.len() {
            64 => (element.as_slice(), TapSighashType::Default),
            65 if element.last() == Some(&SIGHASH_ALL) => (
                element.get(..64).ok_or(SigError::InvalidSignature)?,
                TapSighashType::All,
            ),
            _ => return Err(SigError::InvalidSignature),
        };
        let output_key = message_challenge
            .as_bytes()
            .get(2..34)
            .and_then(|k| XOnlyPublicKey::from_slice(k).ok())
            .ok_or(SigError::InvalidPublicKey)?;
        let sighash = SighashCache::new(&tx)
            .taproot_key_spend_signature_hash(0, &Prevouts::All(&[prevout]), hash_type)
            .map_err(|_| SigError::InvalidSignature)?;
        let sig = schnorr::Signature::from_slice(sig).map_err(|_| SigError::InvalidSignature)?;
        let msg = Message::from_digest(sighash.to_byte_array());
        secp.verify_schnorr(&sig, &msg, &output_key)
            .map_err(|_| SigError::InvalidSignature)
    } else if message_challenge.is_p2wpkh() {
        let [sig, pubkey] = witness.as_slice() else {
            return Err(SigError::InvalidSignature);
        };
        let pubkey = compressed_key(pubkey)?;
        if ScriptBuf::new_p2wpkh(&pubkey.wpubkey_hash()).as_script() != message_challenge {
            return Err(SigError::InvalidSignature);
        }
        let (der, hash_type) = match sig.split_last() {
            Some((&SIGHASH_ALL, der)) => (der, EcdsaSighashType::All),
            _ => return Err(SigError::InvalidSignature),
        };
        let sig = ecdsa::Signature::from_der(der).map_err(|_| SigError::InvalidSignature)?;
        let mut normalized = sig;
        normalized.normalize_s();
        if normalized != sig {
            // A high-S signature is not a standard signature
            return Err(SigError::InvalidSignature);
        }
        let sighash = SighashCache::new(&tx)
            .p2wpkh_signature_hash(0, message_challenge, Amount::ZERO, hash_type)
            .map_err(|_| SigError::InvalidSignature)?;
        let msg = Message::from_digest(sighash.to_byte_array());
        secp.verify_ecdsa(&msg, &sig, &pubkey.0)
            .map_err(|_| SigError::InvalidSignature)
    } else {
        Err(SigError::InvalidSignature)
    }
}

/// The public key in the witness: must be exactly a 33-byte compressed public key. `CompressedPublicKey::from_slice`
/// also accepts the 65-byte uncompressed encoding and treats it as the same key, but P2WPKH takes hash160 of the raw
/// bytes in the witness and the uncompressed encoding corresponds to a different address, so the length is checked
/// first.
fn compressed_key(bytes: &[u8]) -> Result<CompressedPublicKey, SigError> {
    if bytes.len() != 33 {
        return Err(SigError::InvalidPublicKey);
    }
    CompressedPublicKey::from_slice(bytes).map_err(|_| SigError::InvalidPublicKey)
}

/// The locking script `OP_1 <account>` of the account's Taproot address (D-092: the account is the BIP86-tweaked output
/// public key, no further tweak here). Returns [`SigError::InvalidPublicKey`] when the account is not a valid x-only
/// public key (for example a contract ID).
pub fn account_script(account: &[u8; 32]) -> Result<ScriptBuf, SigError> {
    let output_key = XOnlyPublicKey::from_slice(account).map_err(|_| SigError::InvalidPublicKey)?;
    Ok(ScriptBuf::new_p2tr_tweaked(
        TweakedPublicKey::dangerous_assume_tweaked(output_key),
    ))
}

/// Verify account `account`'s BIP322 simple signature over `message`: the address is the account's Taproot address
/// ([`account_script`]), the witness must have exactly 1 item (P2TR key-path), then verify per [`verify_simple`]. Any
/// other witness form is [`SigError::InvalidSignature`] (D-092).
pub fn verify_for_account(
    account: &[u8; 32],
    message: &[u8],
    signature: &[u8],
) -> Result<(), SigError> {
    let witness = decode_witness(signature)?;
    if witness.len() != 1 {
        return Err(SigError::InvalidSignature);
    }
    verify_simple(&account_script(account)?, message, signature)
}

/// Make a BIP322 simple signature over `message` with P2TR key-path (`SIGHASH_DEFAULT`), returning the consensus
/// encoding of the witness. `keypair` is the untweaked internal key: the address is its BIP86-tweaked P2TR (i.e. the
/// account's address), and the signature uses the tweaked key. The signature is determined by
/// `(keypair, message, aux_rand)`.
pub fn sign_simple_p2tr(
    keypair: &Keypair,
    message: &[u8],
    aux_rand: &[u8; 32],
) -> Result<Vec<u8>, SigError> {
    let secp = Secp256k1::new();
    let (internal, _) = keypair.x_only_public_key();
    let challenge = ScriptBuf::new_p2tr(&secp, internal, None);
    let spend = to_spend(&challenge, message);
    let prevout = TxOut {
        value: Amount::ZERO,
        script_pubkey: challenge,
    };
    let tx = to_sign(spend.compute_txid(), Witness::new());
    let sighash = SighashCache::new(&tx)
        .taproot_key_spend_signature_hash(0, &Prevouts::All(&[prevout]), TapSighashType::Default)
        .map_err(|_| SigError::InvalidSignature)?;
    let tweaked = keypair.tap_tweak(&secp, None).to_keypair();
    let sig = secp.sign_schnorr_with_aux_rand(
        &Message::from_digest(sighash.to_byte_array()),
        &tweaked,
        aux_rand,
    );
    Ok(serialize(&Witness::from_slice(&[sig.as_ref().as_slice()])))
}
