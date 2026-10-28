//! Genesis block header (D-027).

use ksana_primitives::{BlockHeader, BtcAnchor, Params, BLOCK_VERSION};

use crate::pow::pow_limit_target;
use crate::ConsensusError;

/// The `entries_root` of the empty entry list: RFC 6962's `MTH({}) = sha256("")` (D-004).
pub fn empty_entries_root() -> [u8; 32] {
    ksana_crypto::merkle_root::<&[u8]>(&[])
}

/// Constructs the genesis block header per D-027: `version = BLOCK_VERSION`, height 0, no parents (D-063), no
/// entries, `producer` all zeros, `bits = pow_limit_bits`, `nonce = 0`. The timestamp, the BTC anchor and the genesis
/// state root (the root of the D-019 pre-allocation) come from the network config and must be identical on all nodes
/// of the same network. The PoW of the genesis block is not checked.
///
/// Returns [`ConsensusError::InvalidPowLimit`] when `params.pow_limit_bits` is not a valid compact target.
pub fn genesis_header(
    params: &Params,
    timestamp: u64,
    btc_anchor: BtcAnchor,
    state_root: [u8; 32],
) -> Result<BlockHeader, ConsensusError> {
    pow_limit_target(params.pow_limit_bits)?;
    Ok(BlockHeader {
        version: BLOCK_VERSION,
        height: 0,
        parents: Vec::new(),
        btc_anchor,
        entries_root: empty_entries_root(),
        state_root,
        producer: [0; 32],
        timestamp,
        bits: params.pow_limit_bits,
        nonce: 0,
    })
}
