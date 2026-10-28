//! Conversion between accounts and Taproot addresses (SPEC §4, D-092): address ↔ account round trips, what the parsing
//! of the textual form accepts and rejects.

use bitcoin::Network;
use ksana_crypto::{
    account_address, account_from_address, account_of, btc_network, keypair_from_secret,
    parse_account, SigError,
};
use ksana_primitives::Network as KsanaNetwork;

#[test]
fn address_round_trips_and_rejects_other_forms() {
    let kp = keypair_from_secret(&[7; 32]).unwrap();
    let account = account_of(&kp);
    for network in [Network::Regtest, Network::Signet, Network::Bitcoin] {
        let address = account_address(&account, network).unwrap().to_string();
        assert_eq!(account_from_address(&address, network), Ok(account));
        assert_eq!(parse_account(&address, network), Ok(account));
        assert_eq!(
            parse_account(&format!(" {} ", address.to_uppercase()), network),
            Ok(account)
        );
        // Not accepted on another network
        let other = if network == Network::Regtest {
            Network::Signet
        } else {
            Network::Regtest
        };
        assert!(matches!(
            account_from_address(&address, other),
            Err(SigError::InvalidAddress(_))
        ));
    }
    let hex = hex::encode(account);
    assert_eq!(parse_account(&hex, Network::Regtest), Ok(account));
    assert_eq!(
        parse_account(&format!("0x{hex}"), Network::Regtest),
        Ok(account)
    );
    // Arbitrary 32 bytes such as a contract ID: hex works, but there is no address
    assert_eq!(
        parse_account(&"ff".repeat(32), Network::Regtest),
        Ok([0xff; 32])
    );
    assert!(account_address(&[0xff; 32], Network::Regtest).is_err());
    // A non-Taproot address, hex of the wrong length, empty input
    assert!(parse_account(
        "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080",
        Network::Regtest
    )
    .is_err());
    assert!(parse_account(&"ab".repeat(31), Network::Regtest).is_err());
    assert!(parse_account("", Network::Regtest).is_err());
    assert!(parse_account("0x", Network::Regtest).is_err());
}

#[test]
fn networks_map_to_btc_prefixes() {
    assert_eq!(btc_network(KsanaNetwork::Devnet), Network::Regtest);
    assert_eq!(btc_network(KsanaNetwork::Signet), Network::Signet);
    assert_eq!(btc_network(KsanaNetwork::Mainnet), Network::Bitcoin);
}
