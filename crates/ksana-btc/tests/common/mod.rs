//! ksana-btc test utilities: deterministic keys and outpoints, producer UTXOs, prevout data sources, settlement
//! payloads.

#![allow(dead_code)]

use std::collections::BTreeMap;

use bitcoin::hashes::{sha256, Hash};
use bitcoin::key::{CompressedPublicKey, Keypair, Secp256k1, TapTweak, XOnlyPublicKey};
use bitcoin::secp256k1::{Message, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, Prevouts, SighashCache, TapSighashType};
use bitcoin::taproot::{LeafVersion, TapLeafHash};
use bitcoin::{ecdsa, taproot, Amount, ScriptBuf, Transaction, TxOut, Txid, Witness};
use ksana_btc::settlement::{self, PrevoutSource, ProducerUtxo, SettlementPair};
use ksana_primitives::{
    Action, BlockHeader, BtcAnchor, EntryData, FeeMode, Payload, SigScheme, VmOp, DEVNET,
};

/// A secret key determined by the tag.
pub fn secret(tag: &str) -> SecretKey {
    SecretKey::from_slice(sha256::Hash::hash(tag.as_bytes()).as_byte_array()).unwrap()
}

/// An outpoint determined by the tag.
pub fn outpoint(tag: &str, vout: u32) -> bitcoin::OutPoint {
    bitcoin::OutPoint {
        txid: Txid::from_byte_array(sha256::Hash::hash(tag.as_bytes()).to_byte_array()),
        vout,
    }
}

/// The P2WPKH locking script of the compressed public key of `secret`.
pub fn p2wpkh_script(secret: &SecretKey) -> ScriptBuf {
    let secp = Secp256k1::new();
    let public = CompressedPublicKey(bitcoin::secp256k1::PublicKey::from_secret_key(
        &secp, secret,
    ));
    ScriptBuf::new_p2wpkh(&public.wpubkey_hash())
}

pub fn txout(value: u64, script: ScriptBuf) -> TxOut {
    TxOut {
        value: Amount::from_sat(value),
        script_pubkey: script,
    }
}

/// The producer's funding key (internal key, untweaked).
pub fn producer_key() -> Keypair {
    Keypair::from_secret_key(&Secp256k1::new(), &secret("producer"))
}

/// The producer's funding / change address.
pub fn producer_script(key: &Keypair) -> ScriptBuf {
    settlement::producer_script(&key.x_only_public_key().0)
}

/// Producer UTXOs belonging to `key`.
pub fn producer_utxos(key: &Keypair, values: &[u64]) -> Vec<ProducerUtxo> {
    values
        .iter()
        .enumerate()
        .map(|(i, &value)| ProducerUtxo {
            outpoint: outpoint("producer utxo", u32::try_from(i).unwrap()),
            txout: txout(value, producer_script(key)),
        })
        .collect()
}

/// The prevouts of the producer UTXOs.
pub fn source(producer: &[ProducerUtxo]) -> BTreeMap<bitcoin::OutPoint, TxOut> {
    producer
        .iter()
        .map(|p| (p.outpoint, p.txout.clone()))
        .collect()
}

/// A data source that always fails (simulates bitcoind being unavailable).
pub struct FailingSource;

impl PrevoutSource for FailingSource {
    fn prevout(&self, _: &bitcoin::OutPoint) -> Result<Option<TxOut>, String> {
        Err("bitcoind unavailable".into())
    }
}

/// A path B VmOp (the signature is a placeholder, the settlement layer does not check it).
pub fn sponsored_op(nonce: u64) -> VmOp {
    VmOp {
        version: 1,
        chain_id: DEVNET.chain_id,
        account: [7; 32],
        nonce,
        fee: FeeMode::Token { amount: 3 },
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: [0; 32],
            to: [9; 32],
            amount: 1,
        },
        sig: SigScheme::Bip340([0x11; 64]),
    }
}

/// A settlement payload: one block header (`nonce` may vary, the length does not) and `ops` path B entries.
pub fn settlement_payload(ops: u64, nonce: u64) -> Vec<u8> {
    let header = BlockHeader {
        version: 1,
        height: 7,
        parents: vec![[1; 32]],
        btc_anchor: BtcAnchor {
            height: 100,
            hash: [2; 32],
        },
        entries_root: [3; 32],
        state_root: [4; 32],
        producer: [5; 32],
        timestamp: 1_700_000_000,
        bits: 0x207f_ffff,
        nonce,
    };
    let entries = (0..ops)
        .map(|n| EntryData::Sponsored {
            op: sponsored_op(n),
        })
        .collect();
    Payload::Settlement {
        headers: vec![header],
        entries,
        bodies: vec![],
    }
    .encode(&DEVNET)
    .unwrap()
}

/// A signed settlement transaction (commit and reveal, D-081) and the material it was built from.
pub struct Settled {
    pub producer: Vec<ProducerUtxo>,
    pub key: Keypair,
    pub payload: Vec<u8>,
    pub pair: SettlementPair,
    /// The prevouts of the commit transaction's inputs.
    pub prevouts: Vec<TxOut>,
    /// The sum of the fees of the two transactions.
    pub fee: u64,
    pub reveal_fee: u64,
    pub change: u64,
}

impl Settled {
    pub fn source(&self) -> BTreeMap<bitcoin::OutPoint, TxOut> {
        source(&self.producer)
    }
}

/// draft → finalize: the payload holds `ops` entries, the producer has two UTXOs, the change goes back to the
/// producer's address.
pub fn settle(ops: u64, feerate: u64) -> Settled {
    settle_with(ops, feerate, &[40_000, 25_000])
}

/// Same as [`settle`], with the amounts of the producer UTXOs given by the caller.
pub fn settle_with(ops: u64, feerate: u64, producer_values: &[u64]) -> Settled {
    let key = producer_key();
    let producer = producer_utxos(&key, producer_values);
    let draft_payload = settlement_payload(ops, 0);
    let draft = settlement::draft(
        &DEVNET,
        &producer,
        producer_script(&key),
        &key.x_only_public_key().0,
        &draft_payload,
        feerate,
    )
    .unwrap();
    let (fee, reveal_fee, change, prevouts) = (
        draft.fee,
        draft.reveal_fee,
        draft.change,
        draft.prevouts.clone(),
    );
    // After PoW solving the header's nonce changes, the payload length does not
    let payload = settlement_payload(ops, 0xDEAD_BEEF);
    assert_eq!(payload.len(), draft_payload.len());
    let pair = settlement::finalize(&DEVNET, draft, &payload, &key).unwrap();
    Settled {
        producer,
        key,
        payload,
        pair,
        prevouts,
        fee,
        reveal_fee,
        change,
    }
}

/// Re-signs the reveal transaction with `key` (untweaked) via the script path with the given sighash type (the leaf in
/// the witness is unchanged); `committed` is the commitment output it spends.
pub fn resign_reveal(
    reveal: &mut Transaction,
    committed: &TxOut,
    key: &Keypair,
    sighash_type: TapSighashType,
) {
    let secp = Secp256k1::new();
    let elements = reveal.input[0].witness.to_vec();
    let leaf_hash = TapLeafHash::from_script(
        bitcoin::Script::from_bytes(&elements[1]),
        LeafVersion::TapScript,
    );
    let sighash = SighashCache::new(&*reveal)
        .taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(std::slice::from_ref(committed)),
            leaf_hash,
            sighash_type,
        )
        .unwrap();
    let signature = secp.sign_schnorr_no_aux_rand(&Message::from(sighash), key);
    let mut elements = elements;
    elements[0] = taproot::Signature {
        signature,
        sighash_type,
    }
    .to_vec();
    reveal.input[0].witness = Witness::from_slice(&elements);
}

/// After the commit transaction was modified: re-signs its inputs (`SIGHASH_DEFAULT`), points the reveal transaction at
/// the new commitment output and re-signs it.
pub fn relink(pair: &mut SettlementPair, prevouts: &[TxOut], key: &Keypair) {
    resign_producer(&mut pair.commit, prevouts, 0, key, TapSighashType::Default);
    pair.reveal.input[0].previous_output = bitcoin::OutPoint {
        txid: pair.commit.compute_txid(),
        vout: settlement::COMMITMENT_OUTPUT,
    };
    let committed = pair.commit.output[1].clone();
    resign_reveal(&mut pair.reveal, &committed, key, TapSighashType::Default);
}

/// Signature verification of the reveal transaction independent of `ksana_btc`: the leaf is taken from the witness, the
/// sighash is computed per BIP342 (the sighash type is taken from the signature), the public key is the internal key
/// from the control block, and the output key of `committed` is checked per BIP341.
pub fn verify_reveal_in_place(reveal: &Transaction, committed: &TxOut) -> Result<(), String> {
    let secp = Secp256k1::verification_only();
    let elements = reveal.input[0].witness.to_vec();
    let [sig, leaf, control] = elements.as_slice() else {
        return Err(format!(
            "reveal transaction witness element count {}",
            elements.len()
        ));
    };
    let control = taproot::ControlBlock::decode(control).map_err(|e| e.to_string())?;
    let leaf = bitcoin::Script::from_bytes(leaf);
    let output_key = XOnlyPublicKey::from_slice(&committed.script_pubkey.as_bytes()[2..])
        .map_err(|e| e.to_string())?;
    if !control.verify_taproot_commitment(&secp, output_key, leaf) {
        return Err("control block does not match the commitment output".into());
    }
    let sig = taproot::Signature::from_slice(sig).map_err(|e| e.to_string())?;
    let sighash = SighashCache::new(reveal)
        .taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(std::slice::from_ref(committed)),
            TapLeafHash::from_script(leaf, control.leaf_version),
            sig.sighash_type,
        )
        .map_err(|e| e.to_string())?;
    secp.verify_schnorr(
        &sig.signature,
        &Message::from(sighash),
        &control.internal_key,
    )
    .map_err(|e| e.to_string())
}

/// Re-signs inputs `first..` with `key` (BIP86 tweaked) with the given sighash type (restores the producer's signatures
/// after a test tampered with the transaction).
pub fn resign_producer(
    tx: &mut Transaction,
    prevouts: &[TxOut],
    first: usize,
    key: &Keypair,
    sighash_type: TapSighashType,
) {
    let secp = Secp256k1::new();
    let tweaked = key.tap_tweak(&secp, None).to_keypair();
    let mut sigs = Vec::new();
    let mut cache = SighashCache::new(&*tx);
    for index in first..tx.input.len() {
        let sighash = cache
            .taproot_key_spend_signature_hash(index, &Prevouts::All(prevouts), sighash_type)
            .unwrap();
        let signature = secp.sign_schnorr_no_aux_rand(&Message::from(sighash), &tweaked);
        sigs.push(
            taproot::Signature {
                signature,
                sighash_type,
            }
            .to_vec(),
        );
    }
    for (txin, sig) in tx.input.iter_mut().skip(first).zip(sigs) {
        txin.witness = Witness::from_slice(&[sig]);
    }
}

/// A BIP143 P2WPKH signature (any sighash type), returns the `DER ‖ type` witness element. `prevout` is the P2WPKH
/// output being spent.
pub fn p2wpkh_sig(
    secret: &SecretKey,
    tx: &Transaction,
    index: usize,
    prevout: &TxOut,
    sighash_type: EcdsaSighashType,
) -> Vec<u8> {
    let secp = Secp256k1::new();
    let sighash = SighashCache::new(tx)
        .p2wpkh_signature_hash(index, &prevout.script_pubkey, prevout.value, sighash_type)
        .unwrap();
    ecdsa::Signature {
        signature: secp.sign_ecdsa(&Message::from(sighash), secret),
        sighash_type,
    }
    .to_vec()
}

/// In-place signature verification independent of `ksana_btc`: as a Bitcoin node does it, computes the sighash over the
/// whole transaction with each input's actual index (BIP341 key-path / BIP143 P2WPKH, the sighash type is taken from
/// the witness) and verifies the signatures of all inputs.
pub fn verify_in_place(tx: &Transaction, prevouts: &[TxOut]) -> Result<(), String> {
    if prevouts.len() != tx.input.len() {
        return Err("prevout count mismatch".into());
    }
    let secp = Secp256k1::verification_only();
    let mut cache = SighashCache::new(tx);
    for (index, (input, prevout)) in tx.input.iter().zip(prevouts).enumerate() {
        let script = &prevout.script_pubkey;
        let witness = input.witness.to_vec();
        if script.is_p2tr() {
            let [element] = witness.as_slice() else {
                return Err(format!(
                    "input {index}: P2TR witness element count {}",
                    witness.len()
                ));
            };
            let sig = taproot::Signature::from_slice(element).map_err(|e| e.to_string())?;
            let key =
                XOnlyPublicKey::from_slice(&script.as_bytes()[2..]).map_err(|e| e.to_string())?;
            let sighash = cache
                .taproot_key_spend_signature_hash(index, &Prevouts::All(prevouts), sig.sighash_type)
                .map_err(|e| e.to_string())?;
            secp.verify_schnorr(&sig.signature, &Message::from(sighash), &key)
                .map_err(|e| format!("input {index}: {e}"))?;
        } else if script.is_p2wpkh() {
            let [sig, public] = witness.as_slice() else {
                return Err(format!(
                    "input {index}: P2WPKH witness element count {}",
                    witness.len()
                ));
            };
            let sig = ecdsa::Signature::from_slice(sig).map_err(|e| e.to_string())?;
            let public =
                bitcoin::CompressedPublicKey::from_slice(public).map_err(|e| e.to_string())?;
            if ScriptBuf::new_p2wpkh(&public.wpubkey_hash()) != *script {
                return Err(format!(
                    "input {index}: public key does not match the witness program"
                ));
            }
            let sighash = cache
                .p2wpkh_signature_hash(index, script, prevout.value, sig.sighash_type)
                .map_err(|e| e.to_string())?;
            secp.verify_ecdsa(&Message::from(sighash), &sig.signature, &public.0)
                .map_err(|e| format!("input {index}: {e}"))?;
        } else {
            return Err(format!("input {index}: unsupported prevout type"));
        }
    }
    Ok(())
}

/// The 33-byte compressed public key of `secret`.
pub fn compressed_pubkey(secret: &SecretKey) -> Vec<u8> {
    bitcoin::secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), secret)
        .serialize()
        .to_vec()
}
