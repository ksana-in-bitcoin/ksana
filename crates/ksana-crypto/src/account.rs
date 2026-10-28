//! Conversion between accounts and Taproot addresses (SPEC §4, D-092).
//!
//! An account is the 32-byte output public key of the user's Taproot address (BIP86 key-path, no script tree); the
//! address is the same bytes written in bech32m (devnet / regtest `bcrt1p…`, signet `tb1p…`, mainnet `bc1p…`). The
//! address that receives KSA and the one that receives BTC are the same, so a wrong paste still lands on the same key.
//! A contract ID is not a key and has no address; it is only written as 64 hex digits (otherwise someone would send
//! BTC to a contract "address").

use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Network};
use ksana_primitives::Network as KsanaNetwork;

use crate::bip322::account_script;
use crate::SigError;

/// The BTC network of a Ksana network (determines the address prefix): devnet runs on regtest, signet runs on signet
/// (D-055).
pub fn btc_network(network: KsanaNetwork) -> Network {
    match network {
        KsanaNetwork::Devnet => Network::Regtest,
        KsanaNetwork::Signet => Network::Signet,
        KsanaNetwork::Mainnet => Network::Bitcoin,
    }
}

/// The Taproot address of an account (the bech32m encoding of `OP_1 <account>`). Returns
/// [`SigError::InvalidPublicKey`] when the account is not a valid x-only public key (for example a contract ID).
pub fn account_address(account: &[u8; 32], network: Network) -> Result<Address, SigError> {
    let script = account_script(account)?;
    Address::from_script(&script, network).map_err(|_| SigError::InvalidPublicKey)
}

/// The account of a Taproot address: the address must belong to `network`, be P2TR (witness v1, 32-byte program) and
/// the program must be a valid x-only public key.
pub fn account_from_address(address: &str, network: Network) -> Result<[u8; 32], SigError> {
    let unchecked: Address<NetworkUnchecked> = address
        .trim()
        .parse()
        .map_err(|e| SigError::InvalidAddress(format!("{e}")))?;
    let address = unchecked
        .require_network(network)
        .map_err(|e| SigError::InvalidAddress(format!("{e}")))?;
    let script = address.script_pubkey();
    if !script.is_p2tr() {
        return Err(SigError::InvalidAddress(
            "not a Taproot (P2TR) address: accounts only correspond to bc1p / tb1p / bcrt1p addresses".into(),
        ));
    }
    let program: [u8; 32] = script
        .as_bytes()
        .get(2..34)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| SigError::InvalidAddress("witness program is not 32 bytes".into()))?;
    // The program must be a point on the curve: otherwise this address cannot be spent on BTC either
    account_script(&program)?;
    Ok(program)
}

/// 64 hex digits → 32 bytes.
fn hex32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let mut out = [0u8; 32];
    for (dst, [hi, lo]) in out.iter_mut().zip(pairs) {
        let hi = char::from(*hi).to_digit(16)?;
        let lo = char::from(*lo).to_digit(16)?;
        *dst = u8::try_from((hi << 4) | lo).ok()?;
    }
    Some(out)
}

/// Parse the textual form of an account: 64 hex digits (any account, including contract IDs; `0x` prefix allowed), or a
/// Taproot address on `network`. Input made up entirely of hex digits is parsed as hex (an address prefix always
/// contains non-hex letters, so there is no confusion).
pub fn parse_account(text: &str, network: Network) -> Result<[u8; 32], SigError> {
    let text = text.trim();
    let hex = text.strip_prefix("0x").unwrap_or(text);
    if !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return hex32(hex).ok_or_else(|| {
            SigError::InvalidAddress(format!(
                "an account must be 64 hex digits or a Taproot address, got {} hex digits",
                hex.len()
            ))
        });
    }
    account_from_address(text, network)
}
