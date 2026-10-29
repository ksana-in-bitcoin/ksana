//! Recognition of native transactions (path C) and forced inclusion (SPEC §7.4, D-036).
//!
//! - **Recognition**: the transaction has exactly one KSN1 carrier and it is an OP_RETURN output (D-032, D-081), the
//!   payload decodes as `Payload::NativeOp { op }` and `op.fee == BtcNative`; coinbase transactions are not native
//!   transactions (D-086: the op's signature binds the output spent by the carrying transaction's first input, and a
//!   coinbase has no such input).
//! - **First input** ([`first_input`]): the signature message of a path-C op includes it (D-086); the execution layer
//!   verifies the signature against it, so an op copied into another transaction cannot pass authentication.
//! - **Window**: `h(x) = x.anchor.height − NATIVE_CONFIRMATIONS + 1` (signed integer, [`inclusion_height`]);
//!   fast-lane block B must include all native transactions confirmed at BTC heights within `(h(parent), h(B)]`
//!   ([`forced_heights`]), in ascending order of BTC height and index within the block.
//! - **Prefix**: the block's entries must start with these forced-inclusion entries (both `btc_txid` and `op` equal),
//!   and no further `Forced` entries may follow ([`check_forced_prefix`]).

use std::ops::RangeInclusive;

use bitcoin::hashes::Hash;
use bitcoin::{Transaction, Txid};
use ksana_crypto::entry_leaf;
use ksana_primitives::{CodecError, Entry, FeeMode, OutPoint, Params, Payload, VmOp};

use crate::data::{tx_data, tx_payload, Carrier, TxData};

/// A native transaction: the BTC transaction ID (internal byte order, D-001), the VmOp it carries, its first input and
/// all its outputs.
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct NativeTx {
    /// The BTC transaction ID (internal byte order).
    pub txid: [u8; 32],
    /// The VmOp carried by the transaction (`fee` is `BtcNative`; the signature is not checked here, the execution
    /// layer verifies it against `first_input`).
    pub op: VmOp,
    /// The output spent by the first input (D-086): the value the op's signature binds.
    pub first_input: OutPoint,
    /// All outputs of the transaction (amount, locking script), in transaction order: path-C ops read them through
    /// the host function `native_tx` (D-078).
    pub outputs: Vec<(u64, Vec<u8>)>,
}

/// The output spent by the transaction's first input (internal byte order, D-001): the value a path-C op's signature
/// binds (D-086). Returns `None` for coinbase transactions (which have no real input).
pub fn first_input(tx: &Transaction) -> Option<OutPoint> {
    if tx.is_coinbase() {
        return None;
    }
    tx.input.first().map(|input| OutPoint {
        txid: input.previous_output.txid.to_byte_array(),
        vout: input.previous_output.vout,
    })
}

/// Converts `outpoint` into the protocol's [`OutPoint`] (internal byte order).
pub fn outpoint_of(outpoint: &bitcoin::OutPoint) -> OutPoint {
    OutPoint {
        txid: outpoint.txid.to_byte_array(),
        vout: outpoint.vout,
    }
}

/// Replaces the KSN1 data output in `tx` with a data output carrying `op` (the other inputs and outputs unchanged),
/// for the construction flow of funding first, then signing the op against the first input, and finally replacing
/// the payload (D-086). `tx` must have exactly one KSN1 carrier and it must be an OP_RETURN output.
pub fn with_native_op(tx: &Transaction, params: &Params, op: &VmOp) -> Result<Transaction, String> {
    let TxData::One {
        carrier: Carrier::Output(vout),
        ..
    } = tx_data(&params.magic, tx)
    else {
        return Err("the transaction does not have exactly one KSN1 data output".into());
    };
    let payload = Payload::NativeOp { op: op.clone() }
        .encode(params)
        .map_err(|e| e.to_string())?;
    let script = crate::data::data_script(&params.magic, &payload).map_err(|e| e.to_string())?;
    let mut tx = tx.clone();
    let output = tx
        .output
        .get_mut(vout as usize)
        .ok_or_else(|| "index of the KSN1 data output is out of range".to_string())?;
    output.script_pubkey = script;
    Ok(tx)
}

/// All outputs of the transaction (amount, locking script), in transaction order.
pub fn tx_outputs(tx: &Transaction) -> Vec<(u64, Vec<u8>)> {
    tx.output
        .iter()
        .map(|o| (o.value.to_sat(), o.script_pubkey.to_bytes()))
        .collect()
}

/// Whether a single transaction is a native transaction: if so, returns its VmOp (recognition rules in the module
/// docs; coinbase transactions are not).
pub fn native_op(params: &Params, tx: &Transaction) -> Option<VmOp> {
    if tx.is_coinbase() {
        return None;
    }
    match tx_payload(params, tx)? {
        (_, Payload::NativeOp { op }) if op.fee == FeeMode::BtcNative => Some(op),
        _ => None,
    }
}

/// All native transactions in a BTC block, in the order of the transactions in the block (coinbase excluded).
pub fn native_txs(params: &Params, block: &bitcoin::Block) -> Vec<NativeTx> {
    block
        .txdata
        .iter()
        .filter_map(|tx| {
            let op = native_op(params, tx)?;
            let first_input = first_input(tx)?;
            Some(NativeTx {
                txid: tx.compute_txid().to_byte_array(),
                op,
                first_input,
                outputs: tx_outputs(tx),
            })
        })
        .collect()
}

/// `h(x) = anchor_height − NATIVE_CONFIRMATIONS + 1`, computed with signed integers (may be negative).
pub fn inclusion_height(params: &Params, anchor_height: u32) -> i64 {
    // The difference of two u32 plus 1 is always within i64; the saturating operations never trigger.
    i64::from(anchor_height)
        .saturating_sub(i64::from(params.native_confirmations))
        .saturating_add(1)
}

/// The BTC heights a block with anchor height `anchor_height` and parent anchor height `parent_anchor_height` must
/// force-include: the intersection of `(h(parent), h(B)]` and `[0, u32::MAX]`; `None` when empty.
pub fn forced_heights(
    params: &Params,
    parent_anchor_height: u32,
    anchor_height: u32,
) -> Option<RangeInclusive<u32>> {
    let low = inclusion_height(params, parent_anchor_height)
        .saturating_add(1)
        .max(0);
    let high = inclusion_height(params, anchor_height).min(i64::from(u32::MAX));
    if low > high {
        return None;
    }
    Some(u32::try_from(low).ok()?..=u32::try_from(high).ok()?)
}

/// Generates forced-inclusion entries `Entry::Forced { btc_txid, op }` in the given order (ascending BTC height,
/// ascending index within the block).
pub fn forced_entries<'a>(txs: impl IntoIterator<Item = &'a NativeTx>) -> Vec<Entry> {
    txs.into_iter().map(forced_entry).collect()
}

/// The leaf hash the forced-inclusion entry of `tx` adds to the `entries_root` of a block that includes it
/// ([`ksana_crypto::entry_leaf`]): the BTC tracker computes it once, when it scans the transaction's BTC block, and
/// computes from it every root over the entries a block must force-include (D-135).
pub fn forced_leaf(tx: &NativeTx) -> Result<[u8; 32], CodecError> {
    entry_leaf(&forced_entry(tx))
}

fn forced_entry(tx: &NativeTx) -> Entry {
    #[cfg(test)]
    probe::FORCED_ENTRIES.with(|built| built.set(built.get().saturating_add(1)));
    Entry::Forced {
        btc_txid: tx.txid,
        op: tx.op.clone(),
    }
}

/// Test hooks, per thread.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    std::thread_local! {
        /// The forced-inclusion entries built from native transactions ([`super::forced_entries`],
        /// [`super::forced_leaf`]).
        pub(crate) static FORCED_ENTRIES: Cell<u64> = const { Cell::new(0) };
    }
}

/// Checks that the block's entries start with `expected` (the result of [`forced_entries`]), equal one by one (both
/// `btc_txid` and `op` equal), and that no `Forced` entry follows. The error message describes the first mismatch.
pub fn check_forced_prefix(entries: &[Entry], expected: &[Entry]) -> Result<(), String> {
    if entries.len() < expected.len() {
        return Err(format!(
            "forced-inclusion entries are incomplete: {} expected, the block has only {} entries",
            expected.len(),
            entries.len()
        ));
    }
    for (index, (got, want)) in entries.iter().zip(expected).enumerate() {
        if got != want {
            return Err(format!(
                "entry {index} should be the forced-inclusion entry {}",
                describe(want)
            ));
        }
    }
    let extra = entries
        .iter()
        .enumerate()
        .skip(expected.len())
        .find(|(_, entry)| matches!(entry, Entry::Forced { .. }));
    if let Some((index, entry)) = extra {
        return Err(format!(
            "entry {index} {} is a superfluous forced-inclusion entry (forced-inclusion entries may only appear at the start and must be exactly the native transactions in the window)",
            describe(entry)
        ));
    }
    Ok(())
}

/// The entry description in error messages: a `Forced` entry gives the BTC transaction ID (in the RPC display byte
/// order).
fn describe(entry: &Entry) -> String {
    match entry {
        Entry::Forced { btc_txid, op } => format!(
            "(BTC transaction {}, account {}, nonce {})",
            Txid::from_byte_array(*btc_txid),
            hex::encode(op.account),
            op.nonce
        ),
        _ => "(not a forced-inclusion entry)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksana_primitives::DEVNET;

    #[test]
    fn inclusion_height_is_signed() {
        let mut params = DEVNET.clone();
        params.native_confirmations = 3;
        assert_eq!(inclusion_height(&params, 0), -2);
        assert_eq!(inclusion_height(&params, 2), 0);
        assert_eq!(inclusion_height(&params, u32::MAX), i64::from(u32::MAX) - 2);
        params.native_confirmations = 0;
        assert_eq!(inclusion_height(&params, u32::MAX), i64::from(u32::MAX) + 1);
        params.native_confirmations = u32::MAX;
        assert_eq!(inclusion_height(&params, 0), 1 - i64::from(u32::MAX));
    }

    #[test]
    fn forced_heights_are_clamped_to_u32() {
        let mut params = DEVNET.clone();
        params.native_confirmations = 0;
        // h(x) = x + 1: an upper bound beyond u32 is clamped to u32::MAX
        assert_eq!(
            forced_heights(&params, u32::MAX - 2, u32::MAX),
            Some(u32::MAX..=u32::MAX)
        );
        assert_eq!(forced_heights(&params, u32::MAX, u32::MAX), None);
    }
}
