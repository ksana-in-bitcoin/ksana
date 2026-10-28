//! KSN1 envelope (D-081): settlement and republish payloads are placed in the leaf script of a Taproot script-path
//! spend, where each witness byte counts as only 1 weight unit (0.25 vB).
//!
//! - **Leaf script**: `<32-byte x-only public key> OP_CHECKSIG OP_FALSE OP_IF <magic> <chunk>… OP_ENDIF`.
//!   `OP_FALSE OP_IF … OP_ENDIF` is not executed; the script's result is the truth value left by the signature check;
//!   the chunks are consecutive [`MAX_CHUNK`]-byte pieces of the payload (the limit of a single push in BTC script),
//!   the last one may be shorter ([`envelope_script`]).
//! - **Commitment**: the output of the commit transaction is a P2TR output containing only this one leaf, with the
//!   internal public key equal to the key in the leaf ([`commitment`]); the witness spending it is
//!   `[signature, leaf script, control block]`. The payload is fixed by the leaf hash committed to by the output, and
//!   the signature also covers the leaf hash, so nobody can replace the payload in the witness.
//!   Witness stack elements not covered by the commitment can be rewritten arbitrarily by relayers (the transaction ID
//!   does not change), so the payload is not placed there.
//! - **Recognition**: when an input's witness, after removing the annex per BIP341 (at least 2 elements, the last one
//!   starting with `0x50`), still has at least 2 elements, the second-to-last is the leaf script ([`witness_leaf`]);
//!   when the leaf script starts with `OP_PUSHBYTES_32 <32 bytes> OP_CHECKSIG OP_FALSE OP_IF` followed by a push whose
//!   data is exactly `magic` (any push encoding), that input carries a KSN1 envelope ([`is_ksn1_envelope`]). After
//!   that only data pushes may follow (`OP_PUSHBYTES_0..75`, `OP_PUSHDATA1/2/4`, non-minimal encodings accepted, each
//!   at most [`MAX_CHUNK`] bytes), ending with exactly `OP_ENDIF` as the end of the script, for the envelope to be
//!   well-formed ([`parse_envelope`]). Recognition looks only at the syntax and does not check the previous output
//!   (there are no previous outputs when scanning BTC blocks).
//!
//! Extraction of the payload from a transaction (OP_RETURN outputs and envelopes taken together "exactly one") is in
//! [`crate::data::tx_data`].

use bitcoin::key::{Secp256k1, XOnlyPublicKey};
use bitcoin::opcodes::all::{OP_CHECKSIG, OP_ENDIF, OP_IF, OP_PUSHBYTES_32};
use bitcoin::opcodes::OP_FALSE;
use bitcoin::script::{PushBytes, Script, ScriptBuf};
use bitcoin::taproot::{LeafVersion, TapLeafHash, TaprootBuilder};
use bitcoin::Witness;

use crate::data::{read_push, DataError, MAX_CHUNK};

const PUSH_32: u8 = OP_PUSHBYTES_32.to_u8();
const CHECKSIG: u8 = OP_CHECKSIG.to_u8();
const FALSE: u8 = OP_FALSE.to_u8();
const IF: u8 = OP_IF.to_u8();
const ENDIF: u8 = OP_ENDIF.to_u8();
/// The BIP341 annex tag: when the witness has at least 2 elements, a last element starting with it is the annex.
const ANNEX_TAG: u8 = 0x50;

/// Builds the leaf script of a KSN1 envelope: `<key> OP_CHECKSIG OP_FALSE OP_IF <magic> <chunk>… OP_ENDIF`.
///
/// The chunks are consecutive [`MAX_CHUNK`]-byte pieces of the payload (the last one may be shorter), written with
/// `push_slice` (which picks the push opcode by length). The result can always be recovered by [`parse_envelope`];
/// the script length depends only on the payload length.
pub fn envelope_script(
    key: &XOnlyPublicKey,
    magic: &[u8; 4],
    payload: &[u8],
) -> Result<ScriptBuf, DataError> {
    if payload.is_empty() {
        return Err(DataError::Empty);
    }
    let mut script = ScriptBuf::new();
    script.push_slice(key.serialize());
    script.push_opcode(OP_CHECKSIG);
    script.push_opcode(OP_FALSE);
    script.push_opcode(OP_IF);
    script.push_slice(magic);
    for chunk in payload.chunks(MAX_CHUNK) {
        let push = <&PushBytes>::try_from(chunk).map_err(|e| DataError::Push(e.to_string()))?;
        script.push_slice(push);
    }
    script.push_opcode(OP_ENDIF);
    Ok(script)
}

/// Whether a leaf script carries a KSN1 envelope: it starts with `OP_PUSHBYTES_32 <public key> OP_CHECKSIG OP_FALSE
/// OP_IF` and a push whose data is exactly `magic` (any push encoding). Only the start is examined: even if the rest
/// is malformed, it is still a KSN1 envelope (and counts towards the "exactly one" decision for the transaction).
pub fn is_ksn1_envelope(magic: &[u8; 4], script: &Script) -> bool {
    envelope_body(magic, script.as_bytes()).is_some()
}

/// Fully parses a KSN1 envelope per D-081, returning the public key in the leaf (32 bytes, not checked to be a valid
/// x-only public key) and the payload formed by concatenating the chunks in order.
///
/// Returns `None` when it is not an envelope, an opcode other than a data push appears, a push is truncated, a chunk
/// exceeds [`MAX_CHUNK`] bytes, the trailing `OP_ENDIF` is missing or bytes follow it.
pub fn parse_envelope(magic: &[u8; 4], script: &Script) -> Option<([u8; 32], Vec<u8>)> {
    let (key, mut rest) = envelope_body(magic, script.as_bytes())?;
    let mut payload = Vec::with_capacity(rest.len());
    loop {
        if rest == [ENDIF] {
            return Some((key, payload));
        }
        let (chunk, tail) = read_push(rest)?;
        if chunk.len() > MAX_CHUNK {
            return None;
        }
        payload.extend_from_slice(chunk);
        rest = tail;
    }
}

/// The leaf script in a witness (BIP341): the second-to-last element when, after removing the annex (at least 2
/// elements, the last one starting with `0x50`), at least 2 elements remain; otherwise `None` (key-path spend, empty
/// witness).
pub fn witness_leaf(witness: &Witness) -> Option<&Script> {
    let annex = witness.len() >= 2 && witness.last().and_then(<[u8]>::first) == Some(&ANNEX_TAG);
    let len = if annex {
        witness.len().checked_sub(1)?
    } else {
        witness.len()
    };
    witness.nth(len.checked_sub(2)?).map(Script::from_bytes)
}

/// The P2TR output committing to the envelope leaf (internal public key `key`, the script tree has `leaf` as its only
/// leaf) and the data needed to spend it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commitment {
    /// The locking script of the P2TR output.
    pub script_pubkey: ScriptBuf,
    /// The control block of the script-path spend (33 bytes: leaf version and output key parity, internal public key;
    /// no Merkle path).
    pub control_block: Vec<u8>,
    /// The leaf hash (the sighash of the script-path signature covers it).
    pub leaf_hash: TapLeafHash,
}

/// Computes the P2TR output committing to `leaf` and the control block (see [`Commitment`]). Fails only on internal
/// errors.
pub fn commitment(key: &XOnlyPublicKey, leaf: &Script) -> Result<Commitment, String> {
    let secp = Secp256k1::verification_only();
    let spend = TaprootBuilder::new()
        .add_leaf(0, leaf.to_owned())
        .map_err(|e| format!("script tree construction failed: {e}"))?
        .finalize(&secp, *key)
        .map_err(|_| "script tree is incomplete".to_string())?;
    let control = spend
        .control_block(&(leaf.to_owned(), LeafVersion::TapScript))
        .ok_or("the script tree has no envelope leaf")?;
    Ok(Commitment {
        script_pubkey: ScriptBuf::new_p2tr_tweaked(spend.output_key()),
        control_block: control.serialize(),
        leaf_hash: TapLeafHash::from_script(leaf, LeafVersion::TapScript),
    })
}

/// If the leaf script starts with the envelope prefix, returns (public key, the bytes after the magic push).
fn envelope_body<'a>(magic: &[u8; 4], script: &'a [u8]) -> Option<([u8; 32], &'a [u8])> {
    let (&push, rest) = script.split_first()?;
    if push != PUSH_32 {
        return None;
    }
    let (key, rest) = rest.split_first_chunk::<32>()?;
    let (ops, rest) = rest.split_first_chunk::<3>()?;
    if *ops != [CHECKSIG, FALSE, IF] {
        return None;
    }
    let (data, rest) = read_push(rest)?;
    (data == magic.as_slice()).then_some((*key, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> XOnlyPublicKey {
        let secp = Secp256k1::new();
        let secret = bitcoin::secp256k1::SecretKey::from_slice(&[0x11; 32]).unwrap();
        bitcoin::key::Keypair::from_secret_key(&secp, &secret)
            .x_only_public_key()
            .0
    }

    #[test]
    fn envelope_layout() {
        let magic = *b"KSN1";
        let script = envelope_script(&key(), &magic, b"abc").unwrap();
        let mut expected = vec![PUSH_32];
        expected.extend(key().serialize());
        expected.extend([CHECKSIG, FALSE, IF, 0x04]);
        expected.extend(magic);
        expected.extend([0x03, b'a', b'b', b'c', ENDIF]);
        assert_eq!(script.as_bytes(), expected.as_slice());
        assert_eq!(
            parse_envelope(&magic, &script),
            Some((key().serialize(), b"abc".to_vec()))
        );
        assert!(envelope_script(&key(), &magic, &[]).is_err());
    }

    #[test]
    fn leaf_follows_bip341_annex_rule() {
        let leaf = vec![0xaa];
        let control = vec![0xc0; 33];
        let sig = vec![1; 64];
        let w = Witness::from_slice(&[sig.clone(), leaf.clone(), control.clone()]);
        assert_eq!(witness_leaf(&w).map(Script::as_bytes), Some(&leaf[..]));
        let annexed = Witness::from_slice(&[sig.clone(), leaf.clone(), control, vec![ANNEX_TAG]]);
        assert_eq!(
            witness_leaf(&annexed).map(Script::as_bytes),
            Some(&leaf[..])
        );
        // key-path (1 element), 1 element plus annex, empty witness: no leaf
        assert_eq!(
            witness_leaf(&Witness::from_slice(std::slice::from_ref(&sig))),
            None
        );
        assert_eq!(
            witness_leaf(&Witness::from_slice(&[sig, vec![ANNEX_TAG]])),
            None
        );
        assert_eq!(witness_leaf(&Witness::new()), None);
    }
}
