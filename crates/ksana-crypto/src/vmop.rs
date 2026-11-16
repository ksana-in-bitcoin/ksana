//! VmOp signature hash, signing and verification (SPEC §4.2): `Bip340` and `Bip322Simple` (D-052, D-092).
//!
//! An account is the output public key of a Taproot address (D-092): `Bip340` verifies with the account as the public
//! key, so the local secret key must first be tweaked per BIP86 before signing ([`tweaked_keypair`]); `Bip322Simple`
//! only accepts P2TR key-path signatures for the address `OP_1 <account>`.
//!
//! A path C op (`fee` is `BtcNative`) is carried by a BTC transaction and its signature is bound to the first input of
//! the carrying transaction (D-086): the signing message is the VmOp's unsigned encoding followed by the `OutPoint`
//! spent by that input (`txid ‖ vout`, 36 bytes). Copying a signed op into another transaction breaks the signature;
//! the copy is an unauthenticated op (does not consume the nonce). The message of a path B op (`Token`) is only the
//! VmOp's unsigned encoding. The two kinds of message differ at byte 46 (the variant index of `fee`), so they cannot be
//! confused.
//!
//! The interfaces are separated by path: `vmop_sighash` / `sign_vmop` / `verify_vmop` only accept path B ops,
//! `native_vmop_sighash` / `sign_native_vmop` / `verify_native_vmop` only accept path C ops and require the first
//! input; misuse returns [`SigError::FirstInputRequired`] or [`SigError::UnexpectedFirstInput`] and never silently
//! yields a verifiable signature.

use ksana_primitives::{Entry, FeeMode, OutPoint, SigScheme, VmOp};

use crate::bip322;
use crate::halfagg::{aggregate, verify_aggregate, HalfSigned};
use crate::schnorr::{account_of, schnorr_sign, schnorr_verify, tweaked_keypair, Keypair};
use crate::tagged::tagged_hash;
use crate::SigError;

/// The BIP340 tag of the VmOp signature hash.
pub const VMOP_SIGHASH_TAG: &str = "Ksana/VmOp/v1";

/// The signing message: the Borsh encoding of the VmOp without `sig`, followed for path C ops by the `OutPoint`
/// encoding of the carrying transaction's first input (D-086). `first_input` must be given for path C ops and only
/// for them.
fn message(op: &VmOp, first_input: Option<&OutPoint>) -> Result<Vec<u8>, SigError> {
    let mut msg = op.encode_unsigned()?;
    match (&op.fee, first_input) {
        (FeeMode::Token { .. }, None) => {}
        (FeeMode::BtcNative, Some(input)) => msg.extend_from_slice(&input.to_bytes()),
        (FeeMode::BtcNative, None) => return Err(SigError::FirstInputRequired),
        (FeeMode::Token { .. }, Some(_)) => return Err(SigError::UnexpectedFirstInput),
    }
    Ok(msg)
}

fn sighash(op: &VmOp, first_input: Option<&OutPoint>) -> Result<[u8; 32], SigError> {
    Ok(tagged_hash(VMOP_SIGHASH_TAG, &message(op, first_input)?))
}

/// The signature hash of a path B op: the tagged hash with tag `Ksana/VmOp/v1` over the Borsh encoding of the VmOp
/// without `sig`. Path C ops use [`native_vmop_sighash`].
pub fn vmop_sighash(op: &VmOp) -> Result<[u8; 32], SigError> {
    sighash(op, None)
}

/// The signature hash of a path C op (D-086): over the Borsh encoding of the VmOp without `sig` followed by the
/// `OutPoint` encoding of the carrying transaction's first input.
pub fn native_vmop_sighash(op: &VmOp, first_input: &OutPoint) -> Result<[u8; 32], SigError> {
    sighash(op, Some(first_input))
}

fn check_account(op: &VmOp, keypair: &Keypair) -> Result<(), SigError> {
    if op.account != account_of(keypair) {
        return Err(SigError::AccountMismatch);
    }
    Ok(())
}

fn sign_bip340(
    op: &VmOp,
    first_input: Option<&OutPoint>,
    keypair: &Keypair,
    aux_rand: &[u8; 32],
) -> Result<SigScheme, SigError> {
    check_account(op, keypair)?;
    let digest = sighash(op, first_input)?;
    let tweaked = tweaked_keypair(keypair)?;
    Ok(SigScheme::Bip340(schnorr_sign(&digest, &tweaked, aux_rand)))
}

fn sign_bip322_p2tr(
    op: &VmOp,
    first_input: Option<&OutPoint>,
    keypair: &Keypair,
    aux_rand: &[u8; 32],
) -> Result<SigScheme, SigError> {
    check_account(op, keypair)?;
    let message = bip322::vmop_message(&sighash(op, first_input)?);
    Ok(SigScheme::Bip322Simple(bip322::sign_simple_p2tr(
        keypair, &message, aux_rand,
    )?))
}

fn verify(op: &VmOp, first_input: Option<&OutPoint>) -> Result<(), SigError> {
    let sighash = sighash(op, first_input)?;
    match &op.sig {
        SigScheme::Bip340(sig) => schnorr_verify(&op.account, &sighash, sig),
        SigScheme::Bip322Simple(sig) => {
            bip322::verify_for_account(&op.account, &bip322::vmop_message(&sighash), sig)
        }
        // Half a signature proves nothing alone: it is checked in its block's aggregate (D-154)
        SigScheme::HalfBip340(_) | SigScheme::HalfBip322(_) | SigScheme::HalfBip322All(_) => {
            Err(SigError::InvalidSignature)
        }
    }
}

/// What a block's aggregate checks of the path B op `op` (D-154): the account, the first half `r` of the signature
/// (`op.sig` may be full or half), and the hash the signature signs: the op's signature hash for `Bip340`, and for
/// `Bip322Simple` BIP341's signature hash of the BIP322 spend ([`bip322::account_sighash`]). A signature without a
/// half form is [`SigError::InvalidSignature`]; a path C op is [`SigError::FirstInputRequired`].
pub fn half_signed(op: &VmOp) -> Result<HalfSigned, SigError> {
    let sighash = vmop_sighash(op)?;
    let bip322 =
        |hash_all| bip322::account_sighash(&op.account, &bip322::vmop_message(&sighash), hash_all);
    let (r, msg) = match op.sig.half().ok_or(SigError::InvalidSignature)? {
        SigScheme::HalfBip340(r) => (r, sighash),
        SigScheme::HalfBip322(r) => (r, bip322(false)?),
        SigScheme::HalfBip322All(r) => (r, bip322(true)?),
        SigScheme::Bip340(_) | SigScheme::Bip322Simple(_) => {
            return Err(SigError::InvalidSignature)
        }
    };
    Ok(HalfSigned {
        pubkey: op.account,
        msg,
        r,
    })
}

/// The entries of a block from the entries a producer chose (D-154): each path B op with its signature in the half
/// form, followed by the aggregate entry when there are path B entries; and the second halves of those signatures in
/// order, which with the block's entries give the signed ops back ([`unseal_entries`]). The signatures are not
/// verified here. `entries` must hold full signatures and no aggregate entry ([`SigError::InvalidSignature`]).
pub fn seal_entries(entries: &[Entry]) -> Result<(Vec<Entry>, Vec<[u8; 32]>), SigError> {
    let mut sealed = Vec::with_capacity(entries.len().saturating_add(1));
    let mut items = Vec::new();
    let mut seconds = Vec::new();
    for entry in entries {
        match entry {
            Entry::Sponsored { op } => {
                let (half, s) = op.split().ok_or(SigError::InvalidSignature)?;
                items.push(half_signed(&half)?);
                seconds.push(s);
                sealed.push(Entry::Sponsored { op: half });
            }
            Entry::Aggregate { .. } => return Err(SigError::InvalidSignature),
            Entry::Forced { .. } | Entry::Unlock { .. } => sealed.push(entry.clone()),
        }
    }
    if !items.is_empty() {
        sealed.push(Entry::Aggregate {
            s: aggregate(&items, &seconds)?,
        });
    }
    Ok((sealed, seconds))
}

/// The path B ops of a block's entries with their full signatures, from the second halves `seconds` a node holds
/// beside the block (D-154): `None` when there is not one second half for each path B entry. The halves are not
/// committed to by the block, so a signature made here is verified before it is relied on.
pub fn unseal_entries(entries: &[Entry], seconds: &[[u8; 32]]) -> Option<Vec<VmOp>> {
    let ops: Vec<&VmOp> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Sponsored { op } => Some(op),
            _ => None,
        })
        .collect();
    if ops.len() != seconds.len() {
        return None;
    }
    ops.into_iter()
        .zip(seconds)
        .map(|(op, s)| op.join(s))
        .collect()
}

/// Verify the aggregate `s` of the path B ops `ops` of one block, in order (D-154): every op carries half a
/// signature, and `s` is the aggregate of signatures by the ops' accounts over what each signs ([`half_signed`]).
pub fn verify_vmops_aggregate<'a>(
    ops: impl IntoIterator<Item = &'a VmOp>,
    s: &[u8; 32],
) -> Result<(), SigError> {
    let items = ops
        .into_iter()
        .map(|op| {
            if !op.sig.is_half() {
                return Err(SigError::InvalidSignature);
            }
            half_signed(op)
        })
        .collect::<Result<Vec<_>, _>>()?;
    verify_aggregate(&items, s)
}

/// BIP340-sign the path B op `op`, returning the value to be written into `op.sig`.
///
/// `keypair` is the untweaked internal key: `op.account` must equal its account ([`account_of`], the BIP86-tweaked
/// x-only public key), and the signature uses the tweaked key; the original value of `op.sig` does not affect the
/// result. Path C ops use [`sign_native_vmop`].
pub fn sign_vmop(op: &VmOp, keypair: &Keypair, aux_rand: &[u8; 32]) -> Result<SigScheme, SigError> {
    sign_bip340(op, None, keypair, aux_rand)
}

/// BIP340-sign the path C op `op` (D-086): `first_input` is the output spent by the first input of the BTC transaction
/// that will carry it.
pub fn sign_native_vmop(
    op: &VmOp,
    first_input: &OutPoint,
    keypair: &Keypair,
    aux_rand: &[u8; 32],
) -> Result<SigScheme, SigError> {
    sign_bip340(op, Some(first_input), keypair, aux_rand)
}

/// Sign the path B op `op` with BIP322 simple (P2TR key-path, the address being the account's Taproot address)
/// (D-052), returning the value to be written into `op.sig`. `keypair` is likewise the untweaked internal key.
pub fn sign_vmop_bip322_p2tr(
    op: &VmOp,
    keypair: &Keypair,
    aux_rand: &[u8; 32],
) -> Result<SigScheme, SigError> {
    sign_bip322_p2tr(op, None, keypair, aux_rand)
}

/// Sign the path C op `op` with BIP322 simple (P2TR key-path), the message bound to `first_input` (D-086).
pub fn sign_native_vmop_bip322_p2tr(
    op: &VmOp,
    first_input: &OutPoint,
    keypair: &Keypair,
    aux_rand: &[u8; 32],
) -> Result<SigScheme, SigError> {
    sign_bip322_p2tr(op, Some(first_input), keypair, aux_rand)
}

/// Verify that a path B op's `op.sig` proves the caller controls the x-only public key `op.account`: `Bip340` verifies
/// the signature hash directly, `Bip322Simple` verifies a BIP322 simple signature over the hex string of the signature
/// hash (only P2TR key-path signatures for the account's Taproot address are accepted, D-052, D-092).
/// Path C ops return [`SigError::FirstInputRequired`]: they must be verified against the carrying transaction with
/// [`verify_native_vmop`].
pub fn verify_vmop(op: &VmOp) -> Result<(), SigError> {
    // A block holds a path B op by half its signature (D-154): one without a half form cannot be included
    if op.sig.split().is_none() {
        return Err(SigError::InvalidSignature);
    }
    verify(op, None)
}

/// Verify the signature of a path C op (D-086): `first_input` is the output spent by the first input of the BTC
/// transaction that actually carries it. An op copied into another transaction fails verification here.
pub fn verify_native_vmop(op: &VmOp, first_input: &OutPoint) -> Result<(), SigError> {
    verify(op, Some(first_input))
}
