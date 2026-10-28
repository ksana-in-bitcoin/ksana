//! Settlement transactions from BTC and P2P (the full path of SPEC §15 "settlement payload decoding"): carrier
//! recognition for a single transaction (OP_RETURN output and envelope in the witness, D-081), native transaction
//! recognition, funding inputs; decoding and validation of the settlement transactions in a block (Borsh
//! `SettlementTxs`: commit and reveal) (`validate_attached`, with the commit transaction's previous outputs treated
//! as the producer's P2TR output and as a P2WPKH output respectively). Path A's PSBT was removed together with path A
//! (D-078).
//!
//! Property: any input can only return an error, never crash.

#![no_main]

use std::collections::BTreeMap;

use bitcoin::key::CompressedPublicKey;
use bitcoin::secp256k1::{Keypair, PublicKey, Secp256k1, SecretKey};
use bitcoin::{Amount, ScriptBuf, TxOut};
use ksana_btc::data::tx_payload;
use ksana_btc::native::native_op;
use ksana_btc::payload::{attached_payload, envelope_payload, funding_inputs};
use ksana_btc::settlement::{
    decode_tx, producer_script, validate_attached, SettlementPair, MAX_SETTLEMENT_TX_BYTES,
};
use ksana_primitives::{SettlementTxs, DEVNET};
use libfuzzer_sys::fuzz_target;

fn secret() -> SecretKey {
    SecretKey::from_slice(&[0x42; 32]).expect("the fixed secret key is valid")
}

fuzz_target!(|data: &[u8]| {
    // Single transaction: any transaction scanned in a BTC block
    if let Ok(tx) = decode_tx(data) {
        let _ = tx_payload(&DEVNET, &tx);
        let _ = native_op(&DEVNET, &tx);
        let _ = funding_inputs(&tx);
        let _ = envelope_payload(&DEVNET, &tx);
    }

    // Settlement transactions in a block: first decode the two byte strings with Borsh, then decode each with the
    // Bitcoin serialization
    let Ok(txs) =
        ksana_primitives::codec::decode::<SettlementTxs>(data, 2 * MAX_SETTLEMENT_TX_BYTES + 8)
    else {
        return;
    };
    let Ok(pair) = SettlementPair::decode(&txs) else {
        return;
    };
    let expected = attached_payload(&DEVNET, &pair).unwrap_or_default();

    // Previous outputs of the commit transaction: the P2TR (BIP86) output of the producer's funding key, or the P2WPKH
    // output of the same secret key, so that validation reaches the signature, envelope and output rules
    let secp = Secp256k1::new();
    let key = Keypair::from_secret_key(&secp, &secret());
    let p2wpkh = CompressedPublicKey(PublicKey::from_secret_key(&secp, &secret()));
    for script_pubkey in [
        producer_script(&key.x_only_public_key().0),
        ScriptBuf::new_p2wpkh(&p2wpkh.wpubkey_hash()),
    ] {
        let funding = TxOut {
            value: Amount::from_sat(1_000_000),
            script_pubkey,
        };
        let prevouts: BTreeMap<bitcoin::OutPoint, TxOut> = pair
            .commit
            .input
            .iter()
            .map(|input| (input.previous_output, funding.clone()))
            .collect();
        let _ = validate_attached(&DEVNET, &pair, &expected, &prevouts);
    }
});
