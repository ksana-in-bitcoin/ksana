//! Settlement transactions (SPEC §7.1, §7.2; D-034, D-037, D-043, D-077, D-081): the correspondence between payloads
//! and block contents lives in `ksana-btc::payload` (shared by the node and the indexer); here their errors are mapped
//! to node errors. The assembly, signing and validation rules of the settlement transactions (commit and reveal) live
//! in `ksana-btc::settlement`.

use ksana_btc::payload::PayloadError;
use ksana_btc::settlement::{SettlementError, SettlementPair};
use ksana_primitives::{BlockHeader, Body, Entry, Params, SettlementTxs};

pub use ksana_btc::payload::{attached_payload, fixed_payload_len, funding_inputs, needs_body};

use crate::NodeError;

fn payload_err(e: PayloadError) -> NodeError {
    match e {
        PayloadError::TooLarge(_) => NodeError::InvalidBlock(e.to_string()),
        PayloadError::Encode(_) => NodeError::Internal(e.to_string()),
    }
}

/// This block's settlement payload (Borsh encoded), see [`ksana_btc::payload::payload`].
pub fn payload(
    params: &Params,
    headers: Vec<BlockHeader>,
    entries: &[Entry],
    bodies: Vec<Body>,
) -> Result<Vec<u8>, NodeError> {
    ksana_btc::payload::payload(params, headers, entries, bodies).map_err(payload_err)
}

/// The number of bytes a block body occupies in the payload (D-077).
pub fn body_len(entries: &[Entry]) -> Result<usize, NodeError> {
    ksana_btc::payload::body_len(entries).map_err(payload_err)
}

/// The number of bytes an entry occupies in the settlement payload.
pub fn entry_data_len(entry: &Entry) -> Result<usize, NodeError> {
    ksana_btc::payload::entry_data_len(entry).map_err(payload_err)
}

/// Decode the settlement transactions carried by a block: commit and reveal, each bounded by `MAX_SETTLEMENT_TX_BYTES`,
/// required to be fully consumed.
pub fn decode(txs: &SettlementTxs) -> Result<SettlementPair, NodeError> {
    SettlementPair::decode(txs).map_err(|e| match e {
        SettlementError::Fatal(m) => NodeError::Internal(m),
        other => NodeError::InvalidBlock(format!("settlement transaction format error: {other}")),
    })
}
