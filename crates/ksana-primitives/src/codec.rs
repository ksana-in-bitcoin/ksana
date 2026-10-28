//! Borsh encoding / decoding entry points.
//!
//! All external input (P2P, RPC, BTC data) is untrusted: the length limit is checked before decoding (SPEC §16),
//! and the input must be fully consumed. Borsh decoding is canonical: `decode(b) == Ok(v)` implies `encode(v) == b`.

use borsh::{BorshDeserialize, BorshSerialize};

use crate::hash::sha256;
use crate::params::Params;
use crate::types::{Payload, VmOp};

/// Encoding / decoding errors.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The input length exceeds the limit; decoding was not attempted.
    #[error("encoded length {len} exceeds the limit {max}")]
    TooLong {
        /// The input length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The input is not a valid Borsh encoding (including trailing bytes).
    #[error("Borsh decode failed: {0}")]
    Decode(String),
    /// Encoding failed (only possible for a Vec / String longer than u32).
    #[error("Borsh encode failed: {0}")]
    Encode(String),
}

/// Borsh encoding.
pub fn encode<T: BorshSerialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    borsh::to_vec(value).map_err(|e| CodecError::Encode(e.to_string()))
}

/// Check `bytes.len() <= max_len` first, then Borsh-decode; the input must be fully consumed.
pub fn decode<T: BorshDeserialize>(bytes: &[u8], max_len: usize) -> Result<T, CodecError> {
    if bytes.len() > max_len {
        return Err(CodecError::TooLong {
            len: bytes.len(),
            max: max_len,
        });
    }
    borsh::from_slice(bytes).map_err(|e| CodecError::Decode(e.to_string()))
}

impl VmOp {
    /// The full Borsh encoding (including `sig`).
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        encode(self)
    }

    /// Decode a single VmOp: the encoded length must not exceed `MAX_OP_BYTES` (SPEC §4.4).
    pub fn decode(bytes: &[u8], params: &Params) -> Result<Self, CodecError> {
        decode(bytes, params.max_op_bytes)
    }

    /// The content of the signing message: the Borsh encoding of the VmOp without the `sig` field (SPEC §4.2).
    ///
    /// `sig` is the last field, so the full encoding = this function's result ‖ `borsh(sig)`.
    pub fn encode_unsigned(&self) -> Result<Vec<u8>, CodecError> {
        let mut buf = Vec::new();
        let written: std::io::Result<()> = (|| {
            self.version.serialize(&mut buf)?;
            self.chain_id.serialize(&mut buf)?;
            self.account.serialize(&mut buf)?;
            self.nonce.serialize(&mut buf)?;
            self.fee.serialize(&mut buf)?;
            self.max_gas.serialize(&mut buf)?;
            self.action.serialize(&mut buf)
        })();
        written.map_err(|e| CodecError::Encode(e.to_string()))?;
        Ok(buf)
    }

    /// `op_id = sha256(full VmOp encoding)` (SPEC §4.4).
    pub fn op_id(&self) -> Result<[u8; 32], CodecError> {
        Ok(sha256(&self.encode()?))
    }
}

impl Payload {
    /// The payload encoding (compressed headers and entries, D-075, D-079, D-080). VmOps in `Settlement` and `Headers`
    /// are written without chain ID: their chain ID must be `params.chain_id`, otherwise encoding fails.
    pub fn encode(&self, params: &Params) -> Result<Vec<u8>, CodecError> {
        let mut out = Vec::new();
        self.write(params.chain_id, &mut out)
            .map_err(|e| CodecError::Encode(e.to_string()))?;
        Ok(out)
    }

    /// Decode a payload from a BTC transaction: the length must not exceed `MAX_BLOCK_DATA_BYTES` (SPEC §7.2; all three
    /// payload kinds use this limit, see DECISIONS D-005), a header sequence holds at most `MAX_SETTLEMENT_HEADERS`
    /// headers (§7.1) and the input must be fully consumed; VmOp chain IDs are taken from `params.chain_id`.
    pub fn decode(bytes: &[u8], params: &Params) -> Result<Self, CodecError> {
        if bytes.len() > params.max_block_data_bytes {
            return Err(CodecError::TooLong {
                len: bytes.len(),
                max: params.max_block_data_bytes,
            });
        }
        let mut reader = bytes;
        let payload = Payload::read(&mut reader, params.chain_id, params.max_settlement_headers)
            .map_err(|e| CodecError::Decode(e.to_string()))?;
        if !reader.is_empty() {
            return Err(CodecError::Decode(format!(
                "{} trailing bytes after the payload",
                reader.len()
            )));
        }
        Ok(payload)
    }
}
