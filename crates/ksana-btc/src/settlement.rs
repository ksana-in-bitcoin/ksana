//! Settlement transactions (SPEC §7.2, §7.3, D-034, D-037, D-081; the user input/output pairs of path A were deleted,
//! D-078): the settlement a block carries consists of two transactions, the commit and the reveal transaction
//! ([`SettlementPair`]); the payload goes in the reveal transaction's witness (a KSN1 envelope, see
//! [`crate::envelope`]).
//!
//! Commit transaction:
//!
//! | Position | Content |
//! | --- | --- |
//! | Inputs | At least one producer input, P2TR key-path or P2WPKH, all `SIGHASH_ALL` (or `SIGHASH_DEFAULT` for P2TR) |
//! | Output 0 | The only one: the commitment output, a P2TR output with only the envelope leaf, the internal key is the key in the leaf |
//!
//! Reveal transaction:
//!
//! | Position | Content |
//! | --- | --- |
//! | Input | Exactly one: output 1 of the commit transaction, empty scriptSig, witness exactly `[signature, envelope leaf, control block]` (signature `SIGHASH_DEFAULT` or `SIGHASH_ALL`) |
//! | Output 0 | The only one: producer change (P2PKH, P2SH, P2WPKH, P2WSH or P2TR, not below the dust floor, not a KSN1 output); only the producer can spend it, so only the producer can CPFP the pair ([`cpfp_child`], D-140) |
//!
//! Both are v2 with `nLockTime = 0` and every input's `nSequence` [`PRODUCER_SEQUENCE`] (no relative time lock,
//! D-100), neither the serialized length nor the weight exceeds [`MAX_SETTLEMENT_TX_BYTES`], and all inputs are
//! distinct; each one's fee (sum of previous output amounts − sum of output amounts) is
//! ≥ `vsize × MIN_SETTLEMENT_FEERATE`, `vsize = ⌈weight / 4⌉`, so each can enter the mempool on its own without
//! relying on CPFP. The envelope leaf must be the canonical construction ([`envelope_script`]) of the payload generated
//! from the block contents.
//!
//! - Producer side: [`draft`] assembles the two transactions and computes the commitment amount and the change by fee
//!   rate; after the PoW is solved, [`finalize`] writes the final payload (same length, so the weight and fee of both
//!   transactions are unchanged), signs the producer inputs of the commit transaction, then signs the reveal
//!   transaction. Producer inputs are always P2TR key-path of the funding key tweaked per BIP86; the key in the
//!   envelope leaf and the internal key of the commitment output are both the funding key itself (untweaked); the
//!   signatures are always `SIGHASH_DEFAULT` (64 bytes, deterministic `sign_schnorr_no_aux_rand`).
//! - Checking: [`validate_attached`] (the settlement transactions a block carries, all rules of D-034 and D-081).
//!   Previous outputs are provided by a [`PrevoutSource`]; a data source fault returns [`SettlementError::Fatal`] and
//!   is never judged invalid. For a transaction already confirmed on BTC, only which headers and bodies its payload
//!   carries matters (D-077); the transaction itself is no longer checked.

use std::collections::{BTreeMap, BTreeSet};

use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::Hash;
use bitcoin::key::{Keypair, Secp256k1, XOnlyPublicKey};
use bitcoin::secp256k1::{schnorr, Message};
use bitcoin::sighash::{EcdsaSighashType, Prevouts, SighashCache, TapSighashType};
use bitcoin::taproot::TapLeafHash;
use bitcoin::{
    absolute, transaction, Amount, OutPoint, Script, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    Witness,
};
use ksana_primitives::{Params, SettlementTxs};

use crate::compress;
use crate::data::is_ksn1_output;
use crate::envelope::{commitment, envelope_script, parse_envelope};
use crate::spend::{
    self, bip86_keypair, expect_tap_sighash, taproot_signature, verify_p2wpkh_spend,
    verify_taproot_key_spend, SpendError,
};

/// The upper limit on the serialized length of a settlement transaction (commit and reveal each) (bytes, the BTC
/// standard transaction limit); the weight must not exceed this number either ([`MAX_SETTLEMENT_WEIGHT`]).
pub const MAX_SETTLEMENT_TX_BYTES: usize = 400_000;

/// The upper limit on the weight of a settlement transaction (commit and reveal each) (WU), the same as
/// [`MAX_SETTLEMENT_TX_BYTES`].
pub const MAX_SETTLEMENT_WEIGHT: u64 = 400_000;

/// The `nSequence` of producer inputs and of the reveal transaction's input: the top bit is 1 (BIP68 not enabled), and
/// it signals replaceability per BIP125.
pub const PRODUCER_SEQUENCE: u32 = 0xFFFF_FFFD;

/// The signature length (bytes) when signing with `SIGHASH_DEFAULT`.
const SIG_LEN: usize = 64;

/// The index of the change output, the only output of the reveal transaction (D-140).
pub const CHANGE_OUTPUT: u32 = 0;

/// The index of the commitment output, the only output of the commit transaction (the reveal transaction spends it).
pub const COMMITMENT_OUTPUT: u32 = 0;

/// The producer's funding and change address: the P2TR locking script of its x-only internal key tweaked per BIP86
/// (no script tree).
pub fn producer_script(internal: &XOnlyPublicKey) -> ScriptBuf {
    ScriptBuf::new_p2tr(&Secp256k1::verification_only(), *internal, None)
}

/// A confirmed UTXO the producer uses for funding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProducerUtxo {
    /// The output reference.
    pub outpoint: OutPoint,
    /// The output (must be the funding key's [`producer_script`]).
    pub txout: TxOut,
}

/// The settlement transactions a block carries (D-081): the commit transaction and the reveal transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettlementPair {
    /// The commit transaction: the producer's funding inputs, with the change and the commitment output as outputs.
    pub commit: Transaction,
    /// The reveal transaction: spends the commitment output and carries the payload out in its witness.
    pub reveal: Transaction,
}

impl SettlementPair {
    /// Decodes the two transactions in a block (each limited to [`MAX_SETTLEMENT_TX_BYTES`] in length, and required to
    /// be fully consumed).
    pub fn decode(txs: &SettlementTxs) -> Result<Self, SettlementError> {
        Ok(SettlementPair {
            commit: decode_tx(&txs.commit)?,
            reveal: decode_tx(&txs.reveal)?,
        })
    }

    /// The form written into the block (Bitcoin network serialization).
    pub fn encode(&self) -> SettlementTxs {
        SettlementTxs {
            commit: serialize(&self.commit),
            reveal: serialize(&self.reveal),
        }
    }

    /// The ID of the reveal transaction (the transaction carrying the payload), internal byte order (D-001).
    pub fn txid(&self) -> [u8; 32] {
        self.reveal.compute_txid().to_byte_array()
    }

    /// The ID of the commit transaction, internal byte order.
    pub fn commit_txid(&self) -> [u8; 32] {
        self.commit.compute_txid().to_byte_array()
    }

    /// The two transactions in broadcast order (parent first).
    pub fn txs(&self) -> [&Transaction; 2] {
        [&self.commit, &self.reveal]
    }
}

/// A settlement transaction draft not yet signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    /// The commit transaction: the producer inputs' witnesses are empty, the commitment output is computed from the
    /// draft's payload ([`finalize`] recomputes it from the final payload), the change is already computed by fee rate.
    pub commit: Transaction,
    /// The reveal transaction: its input does not point at the commit transaction yet, and its witness is a placeholder
    /// of the same length as the final one.
    pub reveal: Transaction,
    /// The previous outputs of the commit transaction's inputs, in input order (the `Prevouts::All` when signing the
    /// producer inputs).
    pub prevouts: Vec<TxOut>,
    /// The payload length: [`finalize`] requires the final payload to have the same length.
    pub payload_len: usize,
    /// The length of the envelope's data ([`compress::pack`], D-141): [`finalize`] fits the final payload to it.
    pub data_len: usize,
    /// The sum of the two transactions' fees (sats): each is the minimum `⌈weight / 4⌉ × fee rate` computed from
    /// the signed weight.
    pub fee: u64,
    /// The reveal transaction's fee (sats); the commitment output's amount is this plus the change.
    pub reveal_fee: u64,
    /// The change amount (sats).
    pub change: u64,
}

/// A construction or checking error of settlement transactions.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettlementError {
    /// The transaction (or the construction parameters) does not follow the settlement rules; when checking, the
    /// settlement transaction is judged invalid.
    #[error("settlement transaction invalid: {0}")]
    Invalid(String),
    /// The producer's funds are insufficient to pay the fee, the commitment amount and a change above dust.
    #[error("producer funds insufficient: {0}")]
    InsufficientFunds(String),
    /// A local fault (the previous output data source failed, internal error): a block must not be judged invalid
    /// because of it.
    #[error("local fault: {0}")]
    Fatal(String),
}

fn invalid(msg: impl Into<String>) -> SettlementError {
    SettlementError::Invalid(msg.into())
}

/// A data source of previous outputs.
///
/// `Ok(None)` means it does not exist in this data source's view: the data source for the settlement transactions a
/// block carries returns only the outputs confirmed on the anchor chain at heights ≤ `anchor.height` (whether or not
/// they were spent later); the data source for confirmed settlements queries txindex. A fault of the data source itself
/// (RPC failure, local chain state lagging behind, etc.) must return `Err`.
pub trait PrevoutSource {
    /// Looks up the output `outpoint` points at.
    fn prevout(&self, outpoint: &OutPoint) -> Result<Option<TxOut>, String>;
}

impl PrevoutSource for BTreeMap<OutPoint, TxOut> {
    fn prevout(&self, outpoint: &OutPoint) -> Result<Option<TxOut>, String> {
        Ok(self.get(outpoint).cloned())
    }
}

/// A summary of settlement transactions that passed checking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The ID of the reveal transaction (carrying the payload) (internal byte order, D-001).
    pub txid: [u8; 32],
    /// The ID of the commit transaction.
    pub commit_txid: [u8; 32],
    /// The sum of the two transactions' fees (sats).
    pub fee: u64,
    /// The sum of the two transactions' virtual sizes (each `⌈weight / 4⌉`).
    pub vsize: u64,
    /// The producer's change amount (sats).
    pub change: u64,
}

/// Assembles a settlement transaction draft per §7.2 and D-081.
///
/// Commit transaction: the inputs are `producer` (`nSequence` is [`PRODUCER_SEQUENCE`], must be P2TR), the only output
/// is the P2TR output committing to the envelope leaf of `key` (payload `payload`, compressed by [`compress::pack`],
/// D-141). Reveal transaction: spends it, with the change to `change_script` as its only output (D-140).
///
/// The reveal transaction's fee = `⌈weight / 4⌉ × feerate_sat_vb` (by the final witness: 64-byte signature, leaf,
/// 33-byte control block); the commit transaction's fee is computed with 64-byte key-path signatures; change = sum of
/// inputs − both fees, the commitment amount = change + the reveal's fee, and when the change is below
/// the dust floor of `change_script`, [`SettlementError::InsufficientFunds`] is returned (likewise when `producer` is
/// empty).
pub fn draft(
    params: &Params,
    producer: &[ProducerUtxo],
    change_script: ScriptBuf,
    key: &XOnlyPublicKey,
    payload: &[u8],
    feerate_sat_vb: u64,
) -> Result<Draft, SettlementError> {
    check_feerate(params, feerate_sat_vb)?;
    if producer.is_empty() {
        return Err(SettlementError::InsufficientFunds(
            "no producer UTXO available".into(),
        ));
    }
    check_payload_len(params, payload)?;
    if is_ksn1_output(&params.magic, &change_script)
        || !spend::is_standard_output_script(&change_script)
    {
        return Err(invalid(
            "the change locking script must be P2PKH, P2SH, P2WPKH, P2WSH or P2TR",
        ));
    }
    if let Some(utxo) = producer
        .iter()
        .find(|utxo| !utxo.txout.script_pubkey.is_p2tr())
    {
        return Err(invalid(format!(
            "producer UTXO {} is not a P2TR output",
            utxo.outpoint
        )));
    }
    let data = compress::pack(payload);
    let leaf = envelope_script(key, &params.magic, &data).map_err(|e| invalid(e.to_string()))?;
    let committed = commitment(key, &leaf).map_err(SettlementError::Fatal)?;

    let input: Vec<TxIn> = producer
        .iter()
        .map(|utxo| TxIn {
            previous_output: utxo.outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence(PRODUCER_SEQUENCE),
            witness: Witness::new(),
        })
        .collect();
    let prevouts: Vec<TxOut> = producer.iter().map(|utxo| utxo.txout.clone()).collect();
    let mut commit = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input,
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: committed.script_pubkey,
        }],
    };
    check_distinct_inputs(&commit)?;

    let mut reveal = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: commit.compute_txid(),
                vout: COMMITMENT_OUTPUT,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence(PRODUCER_SEQUENCE),
            // compute the weight by the final witness: 64-byte signature, leaf, control block
            witness: Witness::from_slice(&[
                vec![0u8; SIG_LEN],
                leaf.into_bytes(),
                committed.control_block,
            ]),
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: change_script,
        }],
    };
    check_size(&reveal, "reveal transaction")?;
    let reveal_fee = reveal
        .weight()
        .to_vbytes_ceil()
        .checked_mul(feerate_sat_vb)
        .ok_or_else(|| invalid("fee overflow"))?;
    let (commit_fee, change) =
        fee_and_change(&commit, &reveal, &prevouts, reveal_fee, feerate_sat_vb)?;
    let committed_value = change
        .checked_add(reveal_fee)
        .ok_or_else(|| invalid("commitment amount overflow"))?;
    set_output(&mut commit, 0, committed_value)?;
    set_output(&mut reveal, 0, change)?;
    let fee = commit_fee
        .checked_add(reveal_fee)
        .ok_or_else(|| invalid("fee overflow"))?;
    Ok(Draft {
        commit,
        reveal,
        prevouts,
        payload_len: payload.len(),
        data_len: data.len(),
        fee,
        reveal_fee,
        change,
    })
}

/// Writes the final payload and signs both transactions, returning the complete settlement transactions.
///
/// `payload` must have the same length as the payload in the draft (PoW solving only changes the header's nonce, not
/// the length), so the weight and fee of both transactions are unchanged. The commitment output is recomputed from the
/// x-only public key of `key` and the final payload; the previous output of every input of the commit transaction must
/// be the [`producer_script`] of `key`, signed with the BIP86-tweaked key using `SIGHASH_DEFAULT` (the sighash covers
/// all previous outputs); then the reveal transaction points at output 1 of the commit transaction and is signed by
/// the script path with `key` itself using `SIGHASH_DEFAULT`.
pub fn finalize(
    params: &Params,
    draft: Draft,
    payload: &[u8],
    key: &Keypair,
) -> Result<SettlementPair, SettlementError> {
    let Draft {
        mut commit,
        mut reveal,
        prevouts,
        payload_len,
        data_len,
        ..
    } = draft;
    if payload.len() != payload_len {
        return Err(invalid(format!(
            "payload length {} differs from the draft's {payload_len}",
            payload.len()
        )));
    }
    let xonly = key.x_only_public_key().0;
    let data = compress::fit(payload, data_len).ok_or_else(|| {
        invalid(format!(
            "the compressed final payload does not fit the draft's {data_len} bytes"
        ))
    })?;
    let leaf = envelope_script(&xonly, &params.magic, &data).map_err(|e| invalid(e.to_string()))?;
    let committed = commitment(&xonly, &leaf).map_err(SettlementError::Fatal)?;
    let committed_out = {
        let out = commit
            .output
            .first_mut()
            .ok_or_else(|| invalid("the draft has no commitment output"))?;
        out.script_pubkey = committed.script_pubkey;
        out.clone()
    };
    sign_producer_inputs(&mut commit, &prevouts, key)?;

    let secp = Secp256k1::new();
    let txin = reveal
        .input
        .first_mut()
        .ok_or_else(|| invalid("the draft's reveal transaction has no input"))?;
    txin.previous_output = OutPoint {
        txid: commit.compute_txid(),
        vout: COMMITMENT_OUTPUT,
    };
    let sighash = SighashCache::new(&reveal)
        .taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(std::slice::from_ref(&committed_out)),
            committed.leaf_hash,
            TapSighashType::Default,
        )
        .map_err(|e| SettlementError::Fatal(format!("sighash computation failed: {e}")))?;
    let signature = secp.sign_schnorr_no_aux_rand(&Message::from(sighash), key);
    if let Some(txin) = reveal.input.first_mut() {
        txin.witness = Witness::from_slice(&[
            signature.serialize().to_vec(),
            leaf.into_bytes(),
            committed.control_block,
        ]);
    }
    Ok(SettlementPair { commit, reveal })
}

/// The split transaction for funding UTXOs (D-075, the node's local policy for maintaining the funding UTXO pool, not
/// a settlement transaction): spends one P2TR UTXO of the producer's funding key and produces `count` outputs of equal
/// amount with the same locking script (a remainder that does not divide evenly goes to the first one); fee =
/// `⌈weight / 4⌉ × feerate_sat_vb`. Returns `None` with fewer than 2 outputs, or when each output would be below
/// `min_value_sat`. v2, `nLockTime = 0`, the input's `nSequence` is the same as the producer inputs of settlement
/// transactions (replaceable).
pub fn split_funding(
    utxo: &ProducerUtxo,
    count: usize,
    min_value_sat: u64,
    feerate_sat_vb: u64,
    key: &Keypair,
) -> Result<Option<Transaction>, SettlementError> {
    if count < 2 {
        return Ok(None);
    }
    let script = utxo.txout.script_pubkey.clone();
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: utxo.outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence(PRODUCER_SEQUENCE),
            // compute the weight by the final witness (64-byte key-path signature)
            witness: Witness::from_slice(&[[0u8; SIG_LEN]]),
        }],
        output: vec![
            TxOut {
                value: Amount::ZERO,
                script_pubkey: script,
            };
            count
        ],
    };
    let vsize = tx.weight().to_vbytes_ceil();
    let fee = vsize
        .checked_mul(feerate_sat_vb)
        .ok_or_else(|| invalid("fee overflow"))?;
    let Some(total) = utxo.txout.value.to_sat().checked_sub(fee) else {
        return Ok(None);
    };
    let n = u64::try_from(count).map_err(|_| invalid("output count overflow"))?;
    let each = total
        .checked_div(n)
        .ok_or_else(|| invalid("output count is 0"))?;
    if each < min_value_sat {
        return Ok(None);
    }
    let first = total
        .checked_rem(n)
        .and_then(|rest| each.checked_add(rest))
        .ok_or_else(|| invalid("amount overflow"))?;
    for (index, out) in tx.output.iter_mut().enumerate() {
        out.value = Amount::from_sat(if index == 0 { first } else { each });
    }
    sign_producer_inputs(&mut tx, std::slice::from_ref(&utxo.txout), key)?;
    Ok(Some(tx))
}

/// The revenue sweep transaction (D-142, the node's local policy for the hot wallet cap of D-060, not a settlement
/// transaction): spends `utxos`, P2TR UTXOs of the producer's funding key, paying `amount_sat` less the fee to `revenue`
/// and the rest back to the funding key's script (a rest below that script's dust floor is raised to it out of the
/// amount, so the funding key never gets back less than the rest; no such output when there is no rest); fee =
/// `⌈weight / 4⌉ × feerate_sat_vb`, out of the revenue output. Returns `None` without inputs, when they do not cover
/// `amount_sat`, or when the revenue output would be below its dust floor. v2, `nLockTime = 0`,
/// the inputs' `nSequence` is [`PRODUCER_SEQUENCE`] (replaceable).
pub fn sweep_revenue(
    utxos: &[ProducerUtxo],
    amount_sat: u64,
    revenue: ScriptBuf,
    feerate_sat_vb: u64,
    key: &Keypair,
) -> Result<Option<Transaction>, SettlementError> {
    if utxos.is_empty() {
        return Ok(None);
    }
    let script = producer_script(&key.x_only_public_key().0);
    let total = utxos
        .iter()
        .try_fold(0u64, |sum, u| sum.checked_add(u.txout.value.to_sat()))
        .ok_or_else(|| invalid("amount overflow"))?;
    let rest = match total.checked_sub(amount_sat) {
        None => return Ok(None),
        Some(0) => 0,
        Some(rest) => rest.max(script.minimal_non_dust().to_sat()),
    };
    let Some(gross) = total.checked_sub(rest) else {
        return Ok(None);
    };
    let revenue_dust = revenue.minimal_non_dust().to_sat();
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: utxos
            .iter()
            .map(|u| TxIn {
                previous_output: u.outpoint,
                script_sig: ScriptBuf::new(),
                sequence: Sequence(PRODUCER_SEQUENCE),
                // compute the weight by the final witness (64-byte key-path signature)
                witness: Witness::from_slice(&[[0u8; SIG_LEN]]),
            })
            .collect(),
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: revenue,
        }],
    };
    if rest > 0 {
        tx.output.push(TxOut {
            value: Amount::from_sat(rest),
            script_pubkey: script,
        });
    }
    let fee = tx
        .weight()
        .to_vbytes_ceil()
        .checked_mul(feerate_sat_vb)
        .ok_or_else(|| invalid("fee overflow"))?;
    let Some(paid) = gross.checked_sub(fee).filter(|&paid| paid >= revenue_dust) else {
        return Ok(None);
    };
    if let Some(out) = tx.output.first_mut() {
        out.value = Amount::from_sat(paid);
    }
    let prevouts: Vec<TxOut> = utxos.iter().map(|u| u.txout.clone()).collect();
    sign_producer_inputs(&mut tx, &prevouts, key)?;
    Ok(Some(tx))
}

/// A child transaction bumping a settlement or republish pair by CPFP through the reveal transaction's change, its only
/// output (the node's local fee policy, D-102, D-140; not a settlement transaction): spends the change, which must be
/// a P2TR output of the funding key `key`, and pays what is left back to the same script. Its fee makes the package
/// (the pair, `pair_fee` sats over `pair_vsize` vB, and the child) pay `feerate_sat_vb`, and is at least `min_fee`
/// (when it replaces an earlier child: that one's fee plus the relay increment) and the child's own
/// `vsize × MIN_SETTLEMENT_FEERATE`. Returns the child and its fee; `None` when the change cannot pay that and stay
/// above the dust floor. v2, `nLockTime = 0`, `nSequence` [`PRODUCER_SEQUENCE`] (replaceable by a later bump).
pub fn cpfp_child(
    params: &Params,
    reveal: &Transaction,
    pair_fee: u64,
    pair_vsize: u64,
    feerate_sat_vb: u64,
    min_fee: u64,
    key: &Keypair,
) -> Result<Option<(Transaction, u64)>, SettlementError> {
    let script = producer_script(&key.x_only_public_key().0);
    let change = reveal
        .output
        .first()
        .filter(|out| out.script_pubkey == script)
        .cloned()
        .ok_or_else(|| {
            invalid("the reveal transaction's change is not a P2TR output of the funding key")
        })?;
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: reveal.compute_txid(),
                vout: CHANGE_OUTPUT,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence(PRODUCER_SEQUENCE),
            // compute the weight by the final witness (64-byte key-path signature)
            witness: Witness::from_slice(&[[0u8; SIG_LEN]]),
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: script.clone(),
        }],
    };
    let vsize = tx.weight().to_vbytes_ceil();
    let package = pair_vsize
        .checked_add(vsize)
        .and_then(|v| v.checked_mul(feerate_sat_vb))
        .ok_or_else(|| invalid("fee overflow"))?;
    let own = vsize
        .checked_mul(params.min_settlement_feerate_sat_vb)
        .ok_or_else(|| invalid("fee overflow"))?;
    let fee = package.saturating_sub(pair_fee).max(min_fee).max(own);
    let dust = script.minimal_non_dust().to_sat();
    let Some(rest) = change
        .value
        .to_sat()
        .checked_sub(fee)
        .filter(|&rest| rest >= dust)
    else {
        return Ok(None);
    };
    if let Some(out) = tx.output.first_mut() {
        out.value = Amount::from_sat(rest);
    }
    sign_producer_inputs(&mut tx, std::slice::from_ref(&change), key)?;
    Ok(Some((tx, fee)))
}

/// Decodes one settlement transaction: first checks that the length does not exceed [`MAX_SETTLEMENT_TX_BYTES`], and
/// requires the input to be fully consumed.
pub fn decode_tx(bytes: &[u8]) -> Result<Transaction, SettlementError> {
    if bytes.len() > MAX_SETTLEMENT_TX_BYTES {
        return Err(invalid(format!(
            "settlement transaction of {} bytes exceeds the limit {MAX_SETTLEMENT_TX_BYTES}",
            bytes.len()
        )));
    }
    bitcoin::consensus::deserialize(bytes)
        .map_err(|e| invalid(format!("settlement transaction decode failed: {e}")))
}

/// Checks the settlement transactions a block carries (all rules of D-034 and D-081). `expected_payload` is the
/// settlement payload generated from the block contents.
///
/// - Both are v2 with `nLockTime = 0`, every input's `nSequence` is [`PRODUCER_SEQUENCE`] (no relative time lock,
///   D-100), and neither the length nor the weight exceeds the limit;
/// - Commit transaction: at least one input, all P2TR key-path (the witness is a 64-byte signature, or 65 bytes with
///   the last byte `0x01`) or P2WPKH (`[DER signature ‖ 0x01, 33-byte compressed public key]`), empty scriptSig, the
///   signatures are verified against all previous outputs; all inputs are distinct, and all previous outputs can be
///   obtained from `source` (`None` means not confirmed before the anchor height, judged invalid); exactly one
///   output, the commitment (see the module docs);
/// - Reveal transaction: exactly one input, spending output 1 of the commit transaction, empty scriptSig, the witness
///   is exactly `[signature, envelope leaf, control block]`: the leaf is the canonical envelope of its data,
///   the control block and the commitment output are both derived from the key in the leaf (the internal key) and
///   this leaf, and the signature is verified by the script path; the envelope's data decompresses (D-141) to
///   `expected_payload`; exactly one output, the change (D-140);
/// - Each transaction's own fee is ≥ `vsize × MIN_SETTLEMENT_FEERATE`.
///
/// The structure and payload rules are checked before the data source is queried.
pub fn validate_attached(
    params: &Params,
    pair: &SettlementPair,
    expected_payload: &[u8],
    source: &dyn PrevoutSource,
) -> Result<Checked, SettlementError> {
    let SettlementPair { commit, reveal } = pair;
    check_payload_len(params, expected_payload)?;

    // structure of the commit transaction
    check_header(commit, "commit transaction")?;
    if commit.input.is_empty() {
        return Err(invalid(
            "the commit transaction must have at least one producer input",
        ));
    }
    check_distinct_inputs(commit)?;
    check_sequences(commit, "commit transaction")?;
    if let Some(index) = commit
        .input
        .iter()
        .position(|txin| !txin.script_sig.is_empty())
    {
        return Err(invalid(format!(
            "the scriptSig of input {index} of the commit transaction must be empty"
        )));
    }
    let committed = check_commit_outputs(commit)?;

    // structure, envelope and signature of the reveal transaction
    check_header(reveal, "reveal transaction")?;
    let [txin] = reveal.input.as_slice() else {
        return Err(invalid(format!(
            "the reveal transaction must have exactly one input, got {}",
            reveal.input.len()
        )));
    };
    let expected_outpoint = OutPoint {
        txid: commit.compute_txid(),
        vout: COMMITMENT_OUTPUT,
    };
    if txin.previous_output != expected_outpoint {
        return Err(invalid(format!(
            "the reveal transaction's input must be the commit transaction's commitment output {expected_outpoint}, got {}",
            txin.previous_output
        )));
    }
    if !txin.script_sig.is_empty() {
        return Err(invalid(
            "the scriptSig of the reveal transaction's input must be empty",
        ));
    }
    check_sequences(reveal, "reveal transaction")?;
    check_reveal_outputs(params, reveal)?;
    let envelope = check_envelope(params, txin, committed, expected_payload)?;
    verify_reveal_signature(reveal, committed, &envelope)?;

    // previous outputs and signatures of the commit transaction
    let prevouts = fetch_prevouts(commit, source)?;
    for index in 0..commit.input.len() {
        verify_producer_input(commit, index, &prevouts)
            .map_err(|e| invalid(format!("input {index} of the commit transaction: {e}")))?;
    }

    // fees: each of the two is not below the floor
    let total_in =
        sum_sat(&prevouts).ok_or_else(|| invalid("sum of previous output amounts overflow"))?;
    let commit_fee = fee_of(params, commit, total_in, "commit transaction")?;
    let reveal_fee = fee_of(
        params,
        reveal,
        committed.value.to_sat(),
        "reveal transaction",
    )?;
    let change = reveal
        .output
        .first()
        .map(|out| out.value.to_sat())
        .ok_or_else(|| invalid("no change output"))?;
    let fee = commit_fee
        .checked_add(reveal_fee)
        .ok_or_else(|| invalid("sum of fees overflow"))?;
    let vsize = commit
        .weight()
        .to_vbytes_ceil()
        .checked_add(reveal.weight().to_vbytes_ceil())
        .ok_or_else(|| invalid("sum of virtual sizes overflow"))?;
    Ok(Checked {
        txid: pair.txid(),
        commit_txid: pair.commit_txid(),
        fee,
        vsize,
        change,
    })
}

/// v2, `nLockTime = 0`, neither the length nor the weight exceeds the limit.
fn check_header(tx: &Transaction, what: &str) -> Result<(), SettlementError> {
    if tx.version != transaction::Version::TWO {
        return Err(invalid(format!(
            "the version of the {what} must be 2, got {}",
            tx.version.0
        )));
    }
    if tx.lock_time.to_consensus_u32() != 0 {
        return Err(invalid(format!(
            "the nLockTime of the {what} must be 0, got {}",
            tx.lock_time.to_consensus_u32()
        )));
    }
    check_size(tx, what)
}

/// Every input's `nSequence` is [`PRODUCER_SEQUENCE`] (D-100): with any other value BIP68 could lock the input
/// (a relative time lock), and the block's settlement could not land until it expires.
fn check_sequences(tx: &Transaction, what: &str) -> Result<(), SettlementError> {
    match tx
        .input
        .iter()
        .position(|txin| txin.sequence != Sequence(PRODUCER_SEQUENCE))
    {
        Some(index) => Err(invalid(format!(
            "the nSequence of input {index} of the {what} must be {PRODUCER_SEQUENCE:#010x}"
        ))),
        None => Ok(()),
    }
}

/// All inputs are distinct.
fn check_distinct_inputs(tx: &Transaction) -> Result<(), SettlementError> {
    let mut seen = BTreeSet::new();
    for (index, txin) in tx.input.iter().enumerate() {
        if !seen.insert(txin.previous_output) {
            return Err(invalid(format!(
                "input {index} ({}) is a duplicate",
                txin.previous_output
            )));
        }
    }
    Ok(())
}

/// The commit transaction's only output, the commitment (D-140), returned. Whether it commits to the right leaf is
/// checked in [`check_envelope`].
fn check_commit_outputs(commit: &Transaction) -> Result<&TxOut, SettlementError> {
    let [committed] = commit.output.as_slice() else {
        return Err(invalid(format!(
            "the commit transaction must have exactly one output, the commitment, got {}",
            commit.output.len()
        )));
    };
    if !committed.script_pubkey.is_p2tr() {
        return Err(invalid(
            "the commit transaction's output must be a P2TR commitment output",
        ));
    }
    Ok(committed)
}

/// The reveal transaction's only output, the producer's change (D-140): P2PKH, P2SH, P2WPKH, P2WSH or P2TR, not a KSN1
/// output, not below its dust floor.
fn check_reveal_outputs(params: &Params, reveal: &Transaction) -> Result<(), SettlementError> {
    let [change] = reveal.output.as_slice() else {
        return Err(invalid(format!(
            "the reveal transaction must have exactly one output, the change, got {}",
            reveal.output.len()
        )));
    };
    if is_ksn1_output(&params.magic, &change.script_pubkey) {
        return Err(invalid("the change output cannot be a KSN1 output"));
    }
    if !spend::is_standard_output_script(&change.script_pubkey) {
        return Err(invalid(
            "the locking script of the change output is not P2PKH, P2SH, P2WPKH, P2WSH or P2TR",
        ));
    }
    let dust = change.script_pubkey.minimal_non_dust().to_sat();
    if change.value.to_sat() < dust {
        return Err(invalid(format!(
            "change of {} sats is below the dust floor of {dust} sats",
            change.value.to_sat()
        )));
    }
    Ok(())
}

/// The envelope and signature in the reveal transaction's witness (the result of [`check_envelope`]).
struct Envelope {
    key: XOnlyPublicKey,
    leaf_hash: TapLeafHash,
    sig: schnorr::Signature,
    sighash_type: TapSighashType,
}

/// The reveal transaction's witness is exactly `[signature, envelope leaf, control block]`: the leaf is the canonical
/// envelope of its data ([`envelope_script`]), which decompresses to `expected_payload` (D-141), the control block and the locking script of `committed` are
/// both derived from the key in the leaf (the internal key) and this leaf; the signature is a BIP340 signature with
/// `SIGHASH_DEFAULT` (64 bytes) or `SIGHASH_ALL` (65 bytes).
fn check_envelope(
    params: &Params,
    txin: &TxIn,
    committed: &TxOut,
    expected_payload: &[u8],
) -> Result<Envelope, SettlementError> {
    let witness: Vec<&[u8]> = txin.witness.iter().collect();
    let [sig, leaf, control] = witness.as_slice() else {
        return Err(invalid(format!(
            "the reveal transaction's witness must be exactly the signature, the envelope leaf and the control block (no annex allowed), got {} elements",
            witness.len()
        )));
    };
    let leaf = Script::from_bytes(leaf);
    let (key, data) = parse_envelope(&params.magic, leaf).ok_or_else(|| {
        invalid("the reveal transaction's witness has no well-formed KSN1 envelope")
    })?;
    // Compared decompressed (D-141): the compressed bytes depend on the compressor
    if compress::unpack(&data, params.max_block_data_bytes).as_deref() != Some(expected_payload) {
        return Err(invalid(
            "the payload in the envelope does not match the block contents",
        ));
    }
    let key = XOnlyPublicKey::from_slice(&key)
        .map_err(|_| invalid("the public key in the envelope is invalid"))?;
    let canonical =
        envelope_script(&key, &params.magic, &data).map_err(|e| invalid(e.to_string()))?;
    if leaf != canonical.as_script() {
        return Err(invalid(
            "the envelope leaf is not the canonical construction of the payload",
        ));
    }
    let expected = commitment(&key, leaf).map_err(SettlementError::Fatal)?;
    if *control != expected.control_block.as_slice() {
        return Err(invalid(
            "the control block does not match the envelope leaf (the internal key must be the key in the leaf, and the script tree has only this leaf)",
        ));
    }
    if committed.script_pubkey != expected.script_pubkey {
        return Err(invalid(
            "the commit transaction's commitment output does not commit to this envelope leaf",
        ));
    }
    let (sig, sighash_type) = taproot_signature(sig)
        .map_err(|e| invalid(format!("the reveal transaction's signature: {e}")))?;
    if sighash_type != TapSighashType::Default {
        expect_tap_sighash(sighash_type, TapSighashType::All)
            .map_err(|e| invalid(format!("the reveal transaction's signature: {e}")))?;
    }
    Ok(Envelope {
        key,
        leaf_hash: expected.leaf_hash,
        sig,
        sighash_type,
    })
}

/// The reveal transaction's script-path signature (BIP342: the sighash covers the leaf hash and the only previous
/// output `committed`), verified with the key in the leaf.
fn verify_reveal_signature(
    reveal: &Transaction,
    committed: &TxOut,
    envelope: &Envelope,
) -> Result<(), SettlementError> {
    let sighash = SighashCache::new(reveal)
        .taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(std::slice::from_ref(committed)),
            envelope.leaf_hash,
            envelope.sighash_type,
        )
        .map_err(|e| {
            invalid(format!(
                "the reveal transaction's sighash cannot be computed: {e}"
            ))
        })?;
    Secp256k1::verification_only()
        .verify_schnorr(&envelope.sig, &Message::from(sighash), &envelope.key)
        .map_err(|_| invalid("the reveal transaction's signature verification failed"))
}

/// The transaction's fee (sum of previous output amounts `total_in` − sum of output amounts), not below
/// `vsize × MIN_SETTLEMENT_FEERATE`.
fn fee_of(
    params: &Params,
    tx: &Transaction,
    total_in: u64,
    what: &str,
) -> Result<u64, SettlementError> {
    let total_out = sum_sat(&tx.output).ok_or_else(|| invalid("sum of output amounts overflow"))?;
    let fee = total_in.checked_sub(total_out).ok_or_else(|| {
        invalid(format!(
            "the sum of output amounts of the {what}, {total_out} sats, exceeds the sum of previous output amounts, {total_in} sats"
        ))
    })?;
    let vsize = tx.weight().to_vbytes_ceil();
    let min_fee = vsize
        .checked_mul(params.min_settlement_feerate_sat_vb)
        .ok_or_else(|| invalid("minimum fee overflow"))?;
    if fee < min_fee {
        return Err(invalid(format!(
            "the fee of the {what}, {fee} sats, is below {vsize} vB × {} sat/vB = {min_fee} sats",
            params.min_settlement_feerate_sat_vb
        )));
    }
    Ok(fee)
}

/// Queries the previous output of each input: `None` (not confirmed before the anchor height) is judged invalid, and a
/// data source fault is a local fault.
fn fetch_prevouts(
    tx: &Transaction,
    source: &dyn PrevoutSource,
) -> Result<Vec<TxOut>, SettlementError> {
    tx.input
        .iter()
        .enumerate()
        .map(
            |(index, txin)| match source.prevout(&txin.previous_output) {
                Ok(Some(prevout)) => Ok(prevout),
                Ok(None) => Err(invalid(format!(
                    "input {index} ({}) not confirmed before the anchor height",
                    txin.previous_output
                ))),
                Err(e) => Err(SettlementError::Fatal(format!(
                    "reading the previous output of input {index} ({}) failed: {e}",
                    txin.previous_output
                ))),
            },
        )
        .collect()
}

/// A producer input: P2TR key-path (`SIGHASH_DEFAULT` or `SIGHASH_ALL`) or P2WPKH (`SIGHASH_ALL`), empty scriptSig,
/// the signature verified against all previous outputs.
fn verify_producer_input(
    tx: &Transaction,
    index: usize,
    prevouts: &[TxOut],
) -> Result<(), SpendError> {
    let (txin, prevout) = tx
        .input
        .get(index)
        .zip(prevouts.get(index))
        .ok_or_else(|| SpendError::Sighash("input index out of range".into()))?;
    let witness: Vec<&[u8]> = txin.witness.iter().collect();
    if prevout.script_pubkey.is_p2tr() {
        let element = match witness.as_slice() {
            [element] => *element,
            other => {
                return Err(SpendError::Witness(format!(
                "a P2TR key-path witness must have exactly 1 element (no annex allowed), got {}",
                other.len()
            )))
            }
        };
        let (sig, sighash_type) = taproot_signature(element)?;
        if sighash_type != TapSighashType::Default {
            expect_tap_sighash(sighash_type, TapSighashType::All)?;
        }
        verify_taproot_key_spend(
            tx,
            index,
            &Prevouts::All(prevouts),
            &prevout.script_pubkey,
            &sig,
            sighash_type,
        )
    } else if prevout.script_pubkey.is_p2wpkh() {
        verify_p2wpkh_spend(tx, index, prevout, &witness, EcdsaSighashType::All)
    } else {
        Err(SpendError::UnsupportedInput)
    }
}

/// Signs all inputs with `key` (BIP86-tweaked) using `SIGHASH_DEFAULT`; the sighash covers all previous outputs.
fn sign_producer_inputs(
    tx: &mut Transaction,
    prevouts: &[TxOut],
    key: &Keypair,
) -> Result<(), SettlementError> {
    if prevouts.len() != tx.input.len() {
        return Err(invalid(format!(
            "{} previous outputs do not match {} inputs",
            prevouts.len(),
            tx.input.len()
        )));
    }
    let secp = Secp256k1::new();
    let own_script = producer_script(&key.x_only_public_key().0);
    let tweaked = bip86_keypair(&secp, key).map_err(SettlementError::Fatal)?;
    let mut signatures = Vec::with_capacity(tx.input.len());
    let mut cache = SighashCache::new(&*tx);
    for (index, prevout) in prevouts.iter().enumerate() {
        if prevout.script_pubkey != own_script {
            return Err(invalid(format!(
                "input {index} is not a P2TR output of the producer's funding key"
            )));
        }
        let sighash = cache
            .taproot_key_spend_signature_hash(
                index,
                &Prevouts::All(prevouts),
                TapSighashType::Default,
            )
            .map_err(|e| SettlementError::Fatal(format!("sighash computation failed: {e}")))?;
        let signature = secp.sign_schnorr_no_aux_rand(&Message::from(sighash), &tweaked);
        signatures.push((index, signature));
    }
    for (index, signature) in signatures {
        if let Some(txin) = tx.input.get_mut(index) {
            txin.witness = Witness::from_slice(&[signature.serialize()]);
        }
    }
    Ok(())
}

/// Computes the commit transaction's fee by fee rate (the weight with 64-byte signatures on the producer inputs) and
/// the change, the reveal transaction's only output: sum of inputs − both fees. Change below its dust floor means
/// insufficient funds.
fn fee_and_change(
    commit: &Transaction,
    reveal: &Transaction,
    prevouts: &[TxOut],
    reveal_fee: u64,
    feerate_sat_vb: u64,
) -> Result<(u64, u64), SettlementError> {
    let mut sized = commit.clone();
    for txin in sized.input.iter_mut() {
        txin.witness = Witness::from_slice(&[[0u8; SIG_LEN]]);
    }
    check_size(&sized, "commit transaction")?;
    let vsize = sized.weight().to_vbytes_ceil();
    let fee = vsize
        .checked_mul(feerate_sat_vb)
        .ok_or_else(|| invalid("fee overflow"))?;
    let total_in = sum_sat(prevouts).ok_or_else(|| invalid("sum of input amounts overflow"))?;
    let change_script = &reveal
        .output
        .first()
        .ok_or_else(|| invalid("no change output"))?
        .script_pubkey;
    let dust = change_script.minimal_non_dust().to_sat();
    let change = total_in
        .checked_sub(fee)
        .and_then(|rest| rest.checked_sub(reveal_fee))
        .filter(|&change| change >= dust)
        .ok_or_else(|| {
            SettlementError::InsufficientFunds(format!(
                "inputs of {total_in} sats cannot pay the fees of {fee} + {reveal_fee} sats ({feerate_sat_vb} sat/vB) and a change of at least {dust} sats"
            ))
        })?;
    Ok((fee, change))
}

fn set_output(tx: &mut Transaction, index: usize, value: u64) -> Result<(), SettlementError> {
    let out = tx
        .output
        .get_mut(index)
        .ok_or_else(|| invalid(format!("no output {index}")))?;
    out.value = Amount::from_sat(value);
    Ok(())
}

fn check_feerate(params: &Params, feerate_sat_vb: u64) -> Result<(), SettlementError> {
    if feerate_sat_vb < params.min_settlement_feerate_sat_vb {
        return Err(invalid(format!(
            "fee rate {feerate_sat_vb} sat/vB is below MIN_SETTLEMENT_FEERATE {} sat/vB",
            params.min_settlement_feerate_sat_vb
        )));
    }
    Ok(())
}

fn check_payload_len(params: &Params, payload: &[u8]) -> Result<(), SettlementError> {
    if payload.len() > params.max_block_data_bytes {
        return Err(invalid(format!(
            "settlement payload of {} bytes exceeds MAX_BLOCK_DATA_BYTES {}",
            payload.len(),
            params.max_block_data_bytes
        )));
    }
    Ok(())
}

fn check_size(tx: &Transaction, what: &str) -> Result<(), SettlementError> {
    let size = tx.total_size();
    let weight = tx.weight().to_wu();
    if size > MAX_SETTLEMENT_TX_BYTES || weight > MAX_SETTLEMENT_WEIGHT {
        return Err(invalid(format!(
            "{what} of {size} bytes and weight {weight} exceeds the limit {MAX_SETTLEMENT_TX_BYTES}"
        )));
    }
    Ok(())
}

fn sum_sat<'a>(outputs: impl IntoIterator<Item = &'a TxOut>) -> Option<u64> {
    outputs
        .into_iter()
        .try_fold(0u64, |total, out| total.checked_add(out.value.to_sat()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_sequence_disables_bip68_and_signals_rbf() {
        // the top bit of `nSequence` (BIP68's disable flag): when set, the input has no relative time lock
        assert_ne!(PRODUCER_SEQUENCE & (1 << 31), 0);
        assert!(Sequence(PRODUCER_SEQUENCE).is_rbf());
    }
}
