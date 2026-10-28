//! KSN1 data carriers (SPEC §7.1, D-032, D-081): OP_RETURN data outputs and envelopes in Taproot witnesses.
//!
//! - **KSN1 output**: a locking script starting with `OP_RETURN` followed by a push whose data is exactly `magic`
//!   (any push encoding).
//! - **Data script**: the rest of a KSN1 output may only be data pushes (`OP_PUSHBYTES_0..75`, `OP_PUSHDATA1/2/4`,
//!   **non-minimal encodings accepted**), each at most [`MAX_CHUNK`] bytes; any other opcode (including
//!   `OP_1`–`OP_16`, `OP_1NEGATE`) or a truncated push is a format error.
//!   The payload is the concatenation of the chunks in order.
//! - **KSN1 envelope** (D-081): the leaf script `<public key> OP_CHECKSIG OP_FALSE OP_IF <magic> <chunk>… OP_ENDIF`
//!   in an input's witness; the rules are in [`crate::envelope`].
//! - **Transaction**: the payload is taken only when the KSN1 outputs and the inputs with KSN1 envelopes taken together
//!   are exactly one, and it is well-formed ([`tx_data`]); the payload's length is checked against
//!   `MAX_BLOCK_DATA_BYTES` first, then it is fully decoded as `Payload`, and the carrier must match the payload kind:
//!   OP_RETURN outputs only carry path-C native transaction payloads, envelopes only carry settlement and republish
//!   payloads ([`tx_payload`], D-005, D-081).
//! - **Construction**: [`data_script`] always writes with rust-bitcoin's `push_slice` (which picks the push opcode by
//!   length), [`MAX_CHUNK`] bytes per chunk, the last one may be shorter.
//!
//! Native transaction recognition, settlement transaction construction and checking, the tracker and the indexer all
//! use this same implementation.

use bitcoin::opcodes::all::{OP_PUSHDATA1, OP_PUSHDATA2, OP_PUSHDATA4, OP_RETURN};
use bitcoin::script::{PushBytes, Script, ScriptBuf};
use bitcoin::Transaction;
use ksana_primitives::{Params, Payload};

use crate::envelope::{is_ksn1_envelope, parse_envelope, witness_leaf};

/// The maximum length of each chunk (bytes, SPEC §7.1; also the limit of a single push in BTC script).
pub const MAX_CHUNK: usize = 520;

const RETURN: u8 = OP_RETURN.to_u8();
const PUSHDATA1: u8 = OP_PUSHDATA1.to_u8();
const PUSHDATA2: u8 = OP_PUSHDATA2.to_u8();
const PUSHDATA4: u8 = OP_PUSHDATA4.to_u8();
/// `OP_PUSHBYTES_75`: the largest push whose length is given directly by the opcode.
const MAX_DIRECT_PUSH: u8 = 0x4b;

/// Errors when constructing a data output.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DataError {
    /// The payload is empty: the encoding of any `Payload` has at least 1 byte, so an empty payload can only be a
    /// caller error.
    #[error("payload is empty")]
    Empty,
    /// A chunk cannot be written into the script (length beyond what a push can express; does not happen for normal
    /// payloads).
    #[error("chunk cannot be written into the script: {0}")]
    Push(String),
}

/// Builds the locking script of a KSN1 data output: `OP_RETURN <magic> <chunk>…`.
///
/// The chunks are consecutive [`MAX_CHUNK`]-byte pieces of the payload (the last one may be shorter), written with
/// `push_slice` (which picks the push opcode by length; a 1-byte chunk is also written as `OP_PUSHBYTES_1`, never as
/// `OP_1`–`OP_16`). The result can always be recovered by [`parse_data_script`].
pub fn data_script(magic: &[u8; 4], payload: &[u8]) -> Result<ScriptBuf, DataError> {
    if payload.is_empty() {
        return Err(DataError::Empty);
    }
    let mut script = ScriptBuf::new();
    script.push_opcode(OP_RETURN);
    script.push_slice(magic);
    for chunk in payload.chunks(MAX_CHUNK) {
        let push = <&PushBytes>::try_from(chunk).map_err(|e| DataError::Push(e.to_string()))?;
        script.push_slice(push);
    }
    Ok(script)
}

/// Whether `script` is a KSN1 output: it starts with `OP_RETURN` followed by a push whose data is exactly `magic`
/// (any push encoding).
///
/// Only the start is examined: even if the rest is malformed, it is still a KSN1 output (and counts towards the
/// "exactly one" decision, see [`tx_data`]).
pub fn is_ksn1_output(magic: &[u8; 4], script: &Script) -> bool {
    ksn1_body(magic, script.as_bytes()).is_some()
}

/// Fully parses a KSN1 data script per D-032, returning the payload formed by concatenating the chunks in order.
///
/// After the `magic` push only data pushes may follow (`OP_PUSHBYTES_0..75`, `OP_PUSHDATA1/2/4`, non-minimal
/// encodings accepted), each at most [`MAX_CHUNK`] bytes; returns `None` when it is not a KSN1 output, another opcode
/// appears (including `OP_1`–`OP_16`, `OP_1NEGATE`) or a push is truncated.
pub fn parse_data_script(magic: &[u8; 4], script: &Script) -> Option<Vec<u8>> {
    let mut rest = ksn1_body(magic, script.as_bytes())?;
    let mut payload = Vec::with_capacity(rest.len());
    while !rest.is_empty() {
        let (chunk, tail) = read_push(rest)?;
        if chunk.len() > MAX_CHUNK {
            return None;
        }
        payload.extend_from_slice(chunk);
        rest = tail;
    }
    Some(payload)
}

/// The position of the KSN1 data within a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carrier {
    /// Output `n` (an OP_RETURN data output).
    Output(u32),
    /// The envelope in the witness of input `n` (D-081).
    Input(u32),
}

/// The KSN1 data in a transaction (D-032, D-081).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxData {
    /// Neither a KSN1 output nor an input with a KSN1 envelope.
    None,
    /// The KSN1 outputs and the inputs with KSN1 envelopes taken together are exactly one, and it is well-formed.
    One {
        /// The position of the carrier.
        carrier: Carrier,
        /// The payload formed by concatenating the chunks in order (not decoded).
        payload: Vec<u8>,
    },
    /// More than one carrier, or the only carrier is malformed: the transaction is not recognized (neither a native
    /// transaction nor a settlement / republish transaction).
    Invalid,
}

/// Extracts the KSN1 data from a transaction: [`TxData::One`] when the KSN1 outputs and the inputs with KSN1 envelopes
/// taken together are exactly one and it is well-formed.
///
/// Other `OP_RETURN` outputs not starting with `magic` and other script-path spends do not count.
pub fn tx_data(magic: &[u8; 4], tx: &Transaction) -> TxData {
    let mut found: Option<(Carrier, &bitcoin::Script)> = None;
    let outputs = tx.output.iter().map(|out| out.script_pubkey.as_script());
    let outputs = (0u32..).zip(outputs).filter_map(|(index, script)| {
        is_ksn1_output(magic, script).then_some((Carrier::Output(index), script))
    });
    let inputs = (0u32..).zip(tx.input.iter()).filter_map(|(index, txin)| {
        let leaf = witness_leaf(&txin.witness)?;
        is_ksn1_envelope(magic, leaf).then_some((Carrier::Input(index), leaf))
    });
    for carrier in outputs.chain(inputs) {
        if found.is_some() {
            return TxData::Invalid;
        }
        found = Some(carrier);
    }
    let Some((carrier, script)) = found else {
        return TxData::None;
    };
    let payload = match carrier {
        Carrier::Output(_) => parse_data_script(magic, script),
        Carrier::Input(_) => parse_envelope(magic, script).map(|(_, payload)| payload),
    };
    match payload {
        Some(payload) => TxData::One { carrier, payload },
        None => TxData::Invalid,
    }
}

/// The KSN1 payload of a transaction: returns `(carrier, payload)` when there is exactly one well-formed carrier
/// ([`tx_data`]), the payload does not exceed `params.max_block_data_bytes`, can be fully decoded as [`Payload`], and
/// its kind matches the carrier (`NativeOp` for an OP_RETURN output, `Settlement` or `Headers` for an envelope,
/// D-081); otherwise `None`.
pub fn tx_payload(params: &Params, tx: &Transaction) -> Option<(Carrier, Payload)> {
    let TxData::One { carrier, payload } = tx_data(&params.magic, tx) else {
        return None;
    };
    let decoded = Payload::decode(&payload, params).ok()?;
    let fits = matches!(
        (&carrier, &decoded),
        (Carrier::Output(_), Payload::NativeOp { .. })
            | (
                Carrier::Input(_),
                Payload::Settlement { .. } | Payload::Headers { .. }
            )
    );
    fits.then_some((carrier, decoded))
}

/// If the script starts with `OP_RETURN <magic>`, returns the bytes after it.
fn ksn1_body<'a>(magic: &[u8; 4], script: &'a [u8]) -> Option<&'a [u8]> {
    let (&first, rest) = script.split_first()?;
    if first != RETURN {
        return None;
    }
    let (data, rest) = read_push(rest)?;
    (data == magic.as_slice()).then_some(rest)
}

/// Reads one data push at the start, returning `(data, the bytes after it)`. Returns `None` when the start is not a
/// data push opcode or the push is truncated.
pub(crate) fn read_push(script: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&opcode, rest) = script.split_first()?;
    let (len, rest) = match opcode {
        0..=MAX_DIRECT_PUSH => (usize::from(opcode), rest),
        PUSHDATA1 => {
            let (&len, rest) = rest.split_first()?;
            (usize::from(len), rest)
        }
        PUSHDATA2 => {
            let (len, rest) = rest.split_first_chunk::<2>()?;
            (usize::from(u16::from_le_bytes(*len)), rest)
        }
        PUSHDATA4 => {
            let (len, rest) = rest.split_first_chunk::<4>()?;
            (usize::try_from(u32::from_le_bytes(*len)).ok()?, rest)
        }
        _ => return None,
    };
    rest.split_at_checked(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_push_handles_every_push_encoding() {
        assert_eq!(read_push(&[0x00, 0xaa]), Some((&[][..], &[0xaa][..])));
        assert_eq!(read_push(&[0x02, 1, 2, 3]), Some((&[1, 2][..], &[3][..])));
        assert_eq!(read_push(&[PUSHDATA1, 1, 7]), Some((&[7][..], &[][..])));
        assert_eq!(read_push(&[PUSHDATA2, 1, 0, 7]), Some((&[7][..], &[][..])));
        assert_eq!(
            read_push(&[PUSHDATA4, 1, 0, 0, 0, 7]),
            Some((&[7][..], &[][..]))
        );
        // truncated: the length field or the data is incomplete
        assert_eq!(read_push(&[]), None);
        assert_eq!(read_push(&[0x03, 1, 2]), None);
        assert_eq!(read_push(&[PUSHDATA1]), None);
        assert_eq!(read_push(&[PUSHDATA2, 1]), None);
        assert_eq!(read_push(&[PUSHDATA4, 0xff, 0xff, 0xff, 0xff, 1]), None);
        // opcodes that are not data pushes
        for opcode in [0x4f, 0x50, 0x51, 0x60, 0x61, RETURN, 0xff] {
            assert_eq!(read_push(&[opcode, 1, 2, 3]), None, "{opcode:#04x}");
        }
    }

    #[test]
    fn magic_may_use_any_push_encoding() {
        let magic = *b"KSN1";
        let mut direct = vec![RETURN, 0x04];
        direct.extend_from_slice(&magic);
        let mut pushdata2 = vec![RETURN, PUSHDATA2, 0x04, 0x00];
        pushdata2.extend_from_slice(&magic);
        for bytes in [direct, pushdata2] {
            assert!(is_ksn1_output(&magic, Script::from_bytes(&bytes)));
            assert_eq!(
                parse_data_script(&magic, Script::from_bytes(&bytes)),
                Some(vec![])
            );
        }
        // the magic push is truncated, or the data is not equal to magic
        assert!(!is_ksn1_output(
            &magic,
            Script::from_bytes(&[RETURN, 0x04, b'K', b'S'])
        ));
        assert!(!is_ksn1_output(
            &magic,
            Script::from_bytes(&[RETURN, 0x05, b'K', b'S', b'N', b'1', 0])
        ));
    }
}
