//! `ksana-keytool`: the devnet key tool (D-042).
//!
//! ```sh
//! ksana-keytool <secret key file>
//! ```
//!
//! When the file does not exist, generates a random secret key (`/dev/urandom`) and saves it as 64 hex digits with
//! permissions 0600; then prints one line of JSON on standard output:
//! `{"account": "<account = BIP86 output public key>", "internal_key": "<untweaked x-only public key>",
//! "funding_script": "<P2TR key-path (BIP86) locking script>", "regtest_address": "<bcrt1p…>",
//! "signet_address": "<tb1p…>"}`.
//! The account is the output public key of the Taproot address (D-092): it can serve as the producer account
//! (`--producer`, the address itself is also accepted) or as an account of the genesis pre-allocation;
//! the same address is used to transfer UTXOs into the settlement funding wallet (`--btc-funding-key-file`).

use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use bitcoin::secp256k1::{Keypair, Secp256k1};
use bitcoin::{Address, Network};
use ksana_btc::settlement::producer_script;

fn load_or_create(path: &Path) -> Result<Keypair, String> {
    let secp = Secp256k1::new();
    if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let mut secret = [0u8; 32];
        hex::decode_to_slice(text.trim(), &mut secret)
            .map_err(|e| format!("secret key must be 64 hex digits: {e}"))?;
        return Keypair::from_seckey_slice(&secp, &secret)
            .map_err(|e| format!("secret key invalid: {e}"));
    }
    let mut urandom =
        std::fs::File::open("/dev/urandom").map_err(|e| format!("opening /dev/urandom: {e}"))?;
    loop {
        let mut secret = [0u8; 32];
        urandom
            .read_exact(&mut secret)
            .map_err(|e| format!("reading /dev/urandom: {e}"))?;
        // The probability of falling outside the curve order is negligible; redraw when it happens
        let Ok(keypair) = Keypair::from_seckey_slice(&secp, &secret) else {
            continue;
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(path)
            .map_err(|e| format!("creating {}: {e}", path.display()))?;
        file.write_all(hex::encode(secret).as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        return Ok(keypair);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        return Err("usage: ksana-keytool <secret key file>".into());
    };
    let keypair = load_or_create(Path::new(&path))?;
    let (internal, _) = keypair.x_only_public_key();
    let script = producer_script(&internal);
    let account = ksana_crypto::account_of(&keypair);
    if ksana_crypto::account_script(&account).ok().as_deref() != Some(script.as_script()) {
        return Err("account does not match the funding script".into());
    }
    let address = |network| {
        Address::from_script(&script, network)
            .map(|a| a.to_string())
            .map_err(|e| format!("cannot derive the address: {e}"))
    };
    let json = serde_json::json!({
        "account": hex::encode(account),
        "internal_key": hex::encode(internal.serialize()),
        "funding_script": hex::encode(script.as_bytes()),
        "regtest_address": address(Network::Regtest)?,
        "signet_address": address(Network::Signet)?,
        "mainnet_address": address(Network::Bitcoin)?,
    });
    println!("{json}");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ksana-keytool: {e}");
            ExitCode::FAILURE
        }
    }
}
