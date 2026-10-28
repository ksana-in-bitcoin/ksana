//! Ksana cryptography: BIP340 tagged hash, Schnorr signatures, VmOp signing and verification (BIP340 and BIP322
//! simple), conversion between accounts and Taproot addresses, Merkle tools (SPEC §4, §4.2, §6.1, §9.3).
//!
//! This crate is consensus code: it must be deterministic, `unwrap` / `panic` are forbidden, errors are always
//! returned.
//! An account is the output public key of a Taproot address (D-092, [`account`], [`schnorr`]); the message, signature
//! encoding and supported address types of `SigScheme::Bip322Simple` are in [`bip322`] (DECISIONS D-052, superseding
//! D-009; only P2TR is accepted since D-092).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::float_arithmetic,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::iter_over_hash_type
)]

pub mod account;
pub mod bip322;
pub mod merkle;
pub mod schnorr;
pub mod tagged;
pub mod vmop;

pub use account::{account_address, account_from_address, btc_network, parse_account};
pub use bip322::account_script;
pub use merkle::{entries_root, merkle_root};
pub use schnorr::{
    account_of, keypair_from_secret, schnorr_sign, schnorr_verify, tweaked_keypair, Keypair,
};
pub use tagged::tagged_hash;
pub use vmop::{
    native_vmop_sighash, sign_native_vmop, sign_native_vmop_bip322_p2tr, sign_vmop,
    sign_vmop_bip322_p2tr, verify_native_vmop, verify_vmop, vmop_sighash, VMOP_SIGHASH_TAG,
};

use ksana_primitives::CodecError;

/// Signing and verification errors.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SigError {
    /// The VmOp cannot be encoded.
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// The secret key is not within the curve order.
    #[error("invalid secret key")]
    InvalidSecretKey,
    /// The public key (account) is not a valid x-only public key.
    #[error("public key is not a valid x-only public key")]
    InvalidPublicKey,
    /// The signature does not match the public key and message.
    #[error("invalid signature")]
    InvalidSignature,
    /// The secret key used for signing does not match the VmOp's account.
    #[error("secret key does not match the VmOp account")]
    AccountMismatch,
    /// A path C op was given without the first input of the carrying transaction (D-086): its signature can only be
    /// made and verified against the carrying transaction.
    #[error("the signature of a path C op is bound to the first input of the carrying transaction, which must be given")]
    FirstInputRequired,
    /// A path B op was given the first input of a carrying transaction: path B ops are not carried by BTC transactions.
    #[error("a path B op is not bound to a BTC transaction input")]
    UnexpectedFirstInput,
    /// The textual form of the account cannot be parsed: neither 64 hex digits nor a Taproot address of this network
    /// (D-092).
    #[error("invalid address: {0}")]
    InvalidAddress(String),
}
