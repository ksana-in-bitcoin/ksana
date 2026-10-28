//! What the token standards KS-20 and KS-721 share (`docs/STANDARDS.md`, D-120): the declaration of the standards a
//! contract follows, the revert reasons both use, the names of the capabilities a contract reports, the limits on names
//! and symbols, the answer of a receiving contract ([`reply`]), and [`AssetRef`], a fungible asset that is either a
//! KS-20 token or a built-in asset.

use alloc::string::String;
use alloc::vec::Vec;

use borsh::BorshSerialize;

use crate::codec::{decode, encode, CallResult, Decode};
use crate::ks20;

/// The zero account: the `from` of a mint and the `to` of a burn in `transfer` events, never a holder.
pub const ZERO_ACCOUNT: [u8; 32] = [0u8; 32];

/// The name of the view, and of the event `init` emits, that declare the standards a contract follows.
pub const STANDARDS: &str = "standards";
/// The name of the view, and of the event `init` emits, that give a token's or a collection's name and symbol.
pub const METADATA: &str = "metadata";
/// The name of the view that reports a contract's capabilities ([`CAPABILITIES`]).
pub const CAPABILITIES_VIEW: &str = "capabilities";

/// Every method a contract does not have reverts with this reason.
pub const ERR_UNKNOWN_METHOD: &[u8] = b"unknown method";
/// A method that takes no attached funds was given some (funds left in a contract that does not expect them could
/// never leave it).
pub const ERR_UNEXPECTED_FUNDS: &[u8] = b"unexpected attached funds";
/// The zero account as a recipient, a spender or an operator.
pub const ERR_ZERO_ACCOUNT: &[u8] = b"zero account";
/// The token's or the collection's own contract as a recipient.
pub const ERR_SELF_RECIPIENT: &[u8] = b"recipient is this contract";
/// `mint` or `set_minter` called by an account that is not the minter.
pub const ERR_NOT_THE_MINTER: &[u8] = b"not the minter";
/// `mint` or `set_minter` after the minter gave the role up for good.
pub const ERR_MINTING_ENDED: &[u8] = b"minting has ended";
/// `init` called on a contract that already has its metadata.
pub const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
/// The metadata is missing: only code running in another contract's storage (a `delegate_call`) can get there.
pub const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
/// A name outside 1 to [`MAX_NAME_BYTES`] bytes.
pub const ERR_NAME: &[u8] = b"name must be 1 to 64 bytes";
/// A symbol outside 1 to [`MAX_SYMBOL_BYTES`] bytes.
pub const ERR_SYMBOL: &[u8] = b"symbol must be 1 to 16 bytes";
/// Another contract answered a standard method with bytes that do not decode as its return type.
pub const ERR_MALFORMED_RETURN: &[u8] = b"malformed return value";
/// An index entry that must exist does not: the storage is inconsistent (never happens).
pub const ERR_MISSING_INDEX: &[u8] = b"missing index entry";
/// Arithmetic that cannot overflow did (never happens).
pub const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";

/// The prefix of the reason of an action refused by a contract's own rules (a freeze, a pause, an allowlist, a limit):
/// `rejected: <reason>`, the same prefix as the refusals of built-in assets' capabilities.
pub const REJECTED: &str = "rejected: ";

/// The longest name, in bytes of UTF-8 (the same rule as built-in assets: the length and valid UTF-8 only, no filter of
/// characters).
pub const MAX_NAME_BYTES: usize = 64;
/// The longest symbol, in bytes of UTF-8.
pub const MAX_SYMBOL_BYTES: usize = 16;

/// Capability: someone can still create more tokens (a minter is set).
pub const CAP_MINT: &str = "mint";
/// Capability: someone can freeze an account's tokens.
pub const CAP_FREEZE: &str = "freeze";
/// Capability: only accounts someone has allowed can hold or move the tokens.
pub const CAP_ALLOWLIST: &str = "allowlist";
/// Capability: someone can stop every transfer.
pub const CAP_PAUSE: &str = "pause";
/// Capability: someone can take tokens from any account.
pub const CAP_CLAWBACK: &str = "clawback";
/// Capability (KS-20 only): transfers pay a fee out of the amount sent.
pub const CAP_FEE: &str = "fee";
/// Capability: the contract's code can be changed (for example a proxy that delegates to code someone can replace).
pub const CAP_UPGRADE: &str = "upgrade";
/// The capabilities `docs/STANDARDS.md` names, in the order a `capabilities()` view lists them. The first four are also
/// the names, in this order, of the issuer capabilities of built-in assets (D-121), so that a wallet marks and sorts
/// both kinds of asset the same way; `clawback`, `fee` and `upgrade` only contracts have.
pub const CAPABILITIES: [&str; 7] = [
    CAP_MINT,
    CAP_FREEZE,
    CAP_ALLOWLIST,
    CAP_PAUSE,
    CAP_CLAWBACK,
    CAP_FEE,
    CAP_UPGRADE,
];

/// The reason of an action the contract's own rules refuse: `rejected: <reason>`.
pub fn rejected(reason: &str) -> Vec<u8> {
    [REJECTED.as_bytes(), reason.as_bytes()].concat()
}

/// Whether a revert reason is a refusal by a contract's own rules ([`rejected`]).
pub fn is_rejection(reason: &[u8]) -> bool {
    reason.starts_with(REJECTED.as_bytes())
}

/// Reverts when funds are attached to the current call.
pub fn no_funds() -> Result<(), Vec<u8>> {
    if crate::call_funds().is_empty() {
        Ok(())
    } else {
        Err(ERR_UNEXPECTED_FUNDS.to_vec())
    }
}

/// Reverts unless `to` can receive tokens: neither the zero account nor this contract.
pub fn check_recipient(to: &[u8; 32]) -> Result<(), Vec<u8>> {
    if *to == ZERO_ACCOUNT {
        Err(ERR_ZERO_ACCOUNT.to_vec())
    } else if *to == crate::self_id() {
        Err(ERR_SELF_RECIPIENT.to_vec())
    } else {
        Ok(())
    }
}

/// Reverts unless `name` is 1 to [`MAX_NAME_BYTES`] bytes.
pub fn check_name(name: &str) -> Result<(), Vec<u8>> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        Err(ERR_NAME.to_vec())
    } else {
        Ok(())
    }
}

/// Reverts unless `symbol` is 1 to [`MAX_SYMBOL_BYTES`] bytes.
pub fn check_symbol(symbol: &str) -> Result<(), Vec<u8>> {
    if symbol.is_empty() || symbol.len() > MAX_SYMBOL_BYTES {
        Err(ERR_SYMBOL.to_vec())
    } else {
        Ok(())
    }
}

/// The answer of `on_token_received` or `on_nft_received`: `value`'s Borsh bytes as the `Bytes` (a Borsh `Vec<u8>`)
/// both methods return, which `transfer_and_call` returns unchanged. `reply(&())` answers empty bytes.
pub fn reply<T: BorshSerialize + ?Sized>(value: &T) -> CallResult {
    crate::ok(&encode(value))
}

/// Whether an allowance or an operator expiring at `expires_at` no longer counts at the current block's timestamp:
/// it counts while `block_timestamp() < expires_at`, and always when `expires_at` is `None`.
pub fn expired(expires_at: Option<u64>) -> bool {
    expires_at.is_some_and(|at| crate::block_timestamp() >= at)
}

/// The names in `present` in the order of [`CAPABILITIES`], names it does not know last, each once.
pub(crate) fn ordered(present: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = CAPABILITIES
        .iter()
        .filter(|name| present.contains(name))
        .map(|name| String::from(*name))
        .collect();
    for name in present {
        if !CAPABILITIES.contains(name) && !out.iter().any(|s| s == name) {
            out.push(String::from(*name));
        }
    }
    out
}

/// A standard method: refuses attached funds, decodes `args` as `A`, runs `f` and encodes its result.
pub(crate) fn run<A: Decode, R: BorshSerialize>(
    args: &[u8],
    f: impl FnOnce(A) -> Result<R, Vec<u8>>,
) -> CallResult {
    no_funds()?;
    let a = decode(args)?;
    crate::ok(&f(a)?)
}

/// Decodes another contract's answer as `T`, reverting with [`ERR_MALFORMED_RETURN`] when it does not decode.
pub(crate) fn decode_return<T: Decode>(bytes: &[u8]) -> Result<T, Vec<u8>> {
    decode(bytes).map_err(|_| ERR_MALFORMED_RETURN.to_vec())
}

/// The standards `contract` declares (its `standards()` view), or `None` when it is no contract or does not answer with
/// a list of names. A catchable call ([`crate::try_call`]): the call's gas is spent either way.
pub fn standards_of(contract: &[u8; 32]) -> Option<Vec<String>> {
    let bytes = crate::try_call(contract, STANDARDS, &[]).ok()?;
    decode(&bytes).ok()
}

/// Whether `contract` declares the standard `id` (for example [`ks20::ID`]).
pub fn declares(contract: &[u8; 32], id: &str) -> bool {
    standards_of(contract).is_some_and(|ids| ids.iter().any(|s| s == id))
}

/// A fungible asset of either kind: a KS-20 token contract or a built-in asset (KSA is the built-in asset of 32 zero
/// bytes). The Borsh encoding is the variant's index (`Token` 0, `Asset` 1) and then the 32-byte ID, the same as the
/// example AMM's `Side`, so a contract can take either kind of asset in one argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize)]
pub enum AssetRef {
    /// A KS-20 token: the token contract's ID.
    Token([u8; 32]),
    /// A built-in asset: its asset ID.
    Asset([u8; 32]),
}

impl Decode for AssetRef {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(Self::Token(<[u8; 32]>::decode(input)?)),
            1 => Some(Self::Asset(<[u8; 32]>::decode(input)?)),
            _ => None,
        }
    }
}

impl AssetRef {
    /// The token contract's ID or the built-in asset's ID.
    pub fn id(&self) -> [u8; 32] {
        match self {
            Self::Token(id) | Self::Asset(id) => *id,
        }
    }

    /// The balance `account` holds: the token's `balance_of` or the built-in balance.
    pub fn balance_of(&self, account: &[u8; 32]) -> Result<u128, Vec<u8>> {
        match self {
            Self::Token(token) => ks20::Token(*token).balance_of(account),
            Self::Asset(asset) => Ok(crate::asset_balance(asset, account)),
        }
    }

    /// Pays `amount` from **this contract's** balance to `to`: the token's `transfer` or `asset_transfer`. The whole op
    /// fails when the payment does (insufficient balance, the token's own rules). A KS-20 token that charges a fee gives
    /// `to` less than `amount` (`docs/STANDARDS.md`, KS-20 rule 4); this contract's balance goes down by `amount` either
    /// way.
    pub fn pay(&self, to: &[u8; 32], amount: u128) {
        match self {
            Self::Token(token) => ks20::Token(*token).transfer(to, amount),
            Self::Asset(asset) => crate::asset_transfer(asset, to, amount),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn asset_ref_encoding_matches_the_amm_side() {
        let token = AssetRef::Token([7; 32]);
        let asset = AssetRef::Asset([9; 32]);
        assert_eq!(encode(&token), encode(&(0u8, [7u8; 32])));
        assert_eq!(encode(&asset), encode(&(1u8, [9u8; 32])));
        assert_eq!(decode::<AssetRef>(&encode(&token)), Ok(token));
        assert_eq!(decode::<AssetRef>(&encode(&asset)), Ok(asset));
        assert!(decode::<AssetRef>(&encode(&(2u8, [9u8; 32]))).is_err());
        assert_eq!(token.id(), [7; 32]);
        assert_eq!(asset.id(), [9; 32]);
    }

    #[test]
    fn capabilities_keep_one_order() {
        assert_eq!(
            ordered(&["upgrade", "own rule", "fee", "mint", "fee"]),
            vec![
                String::from("mint"),
                String::from("fee"),
                String::from("upgrade"),
                String::from("own rule")
            ]
        );
        assert!(ordered(&[]).is_empty());
    }

    #[test]
    fn a_reply_is_bytes() {
        assert_eq!(reply(&7u128), Ok(encode(&encode(&7u128))));
        assert_eq!(reply(&()), Ok(vec![0, 0, 0, 0]));
    }

    #[test]
    fn rejections_carry_the_prefix() {
        assert_eq!(
            rejected("account is frozen"),
            b"rejected: account is frozen".to_vec()
        );
        assert!(is_rejection(&rejected("paused")));
        assert!(!is_rejection(b"insufficient balance"));
    }

    #[test]
    fn names_and_symbols_are_counted_in_bytes() {
        assert!(check_name("").is_err());
        assert!(check_name(&"n".repeat(64)).is_ok());
        assert!(check_name(&"n".repeat(65)).is_err());
        // eight two-byte characters are 16 bytes, nine are 18
        assert!(check_symbol(&"ü".repeat(8)).is_ok());
        assert!(check_symbol(&"ü".repeat(9)).is_err());
        assert!(check_symbol("").is_err());
    }
}
