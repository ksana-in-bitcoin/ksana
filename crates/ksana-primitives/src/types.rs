//! Consensus data types (SPEC §4.1, §6.1, §7.1).
//!
//! All types are Borsh-encoded: structs concatenate their fields in declaration order, enums first write a 1-byte
//! variant index (from 0 in declaration order), `Vec` / `String` first write a u32 little-endian length, `Option`
//! first writes a 1-byte tag. The field order and variant order are the encoding format; changing them is a
//! consensus change.
//!
//! All BTC transaction IDs / block hashes (`[u8; 32]`) are stored in Bitcoin internal byte order,
//! i.e. the raw sha256d output, the reverse of the hex byte order shown by RPC and block explorers (DECISIONS D-001).

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::hash::sha256d;
use crate::json::{dec_u128, dec_u64, hex_array, hex_array_vec, hex_bytes};

/// A Bitcoin transaction output reference (not defined by the SPEC, see DECISIONS D-001).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutPoint {
    /// Transaction ID (internal byte order).
    #[serde(with = "hex_array")]
    pub txid: [u8; 32],
    /// Output index.
    pub vout: u32,
}

impl OutPoint {
    /// Borsh encoding: `txid ‖ vout` (little-endian), 36 bytes. The signing message of a path C op ends with this
    /// encoding of the carrying transaction's first input (D-086).
    pub fn to_bytes(&self) -> [u8; 36] {
        let mut out = [0u8; 36];
        let (txid, vout) = out.split_at_mut(32);
        txid.copy_from_slice(&self.txid);
        vout.copy_from_slice(&self.vout.to_le_bytes());
        out
    }
}

/// A VM operation signed by a user (SPEC §4.1).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmOp {
    /// Currently 2 ([`crate::VMOP_VERSION`]; from 2 on, `Call` / `Deploy` carry `funds`, D-073).
    pub version: u8,
    /// Distinguishes devnet / testnet / mainnet, preventing cross-network replay.
    pub chain_id: u32,
    /// The account: an x-only public key.
    #[serde(with = "hex_array")]
    pub account: [u8; 32],
    /// Strictly increasing from 0 within the account.
    #[serde(with = "dec_u64")]
    pub nonce: u64,
    /// The fee mode (SPEC §8).
    pub fee: FeeMode,
    /// The execution limit; exceeding it fails the op but the fee is charged as usual.
    #[serde(with = "dec_u64")]
    pub max_gas: u64,
    /// The action to execute.
    pub action: Action,
    /// The signature; not part of the signature hash.
    pub sig: SigScheme,
}

/// The fee mode, corresponding to the two submission paths (SPEC §4.1, §5; path A is deprecated, D-078).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum FeeMode {
    /// Path B: sponsored, paying with the project token.
    Token {
        /// The token amount.
        #[serde(with = "dec_u128")]
        amount: u128,
    },
    /// Path C: native transaction.
    BtcNative,
}

/// The action a VmOp executes (SPEC §4.1).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Action {
    /// Deploy a contract.
    Deploy {
        /// WASM bytecode.
        #[serde(with = "hex_bytes")]
        code: Vec<u8>,
        /// The arguments of `init`.
        #[serde(with = "hex_bytes")]
        init_args: Vec<u8>,
        /// Built-in assets transferred to the new contract with the deployment (D-073).
        funds: Vec<Funds>,
    },
    /// Call a contract.
    Call {
        /// The contract ID.
        #[serde(with = "hex_array")]
        contract: [u8; 32],
        /// The method name.
        method: String,
        /// The method arguments.
        #[serde(with = "hex_bytes")]
        args: Vec<u8>,
        /// Built-in assets transferred to the contract with the call (a multi-asset `msg.value`, D-073).
        funds: Vec<Funds>,
    },
    /// Transfer a VM built-in asset.
    AssetTransfer {
        /// The asset ID.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The receiving account.
        #[serde(with = "hex_array")]
        to: [u8; 32],
        /// The amount.
        #[serde(with = "dec_u128")]
        amount: u128,
    },
}

/// One built-in asset attached to a call (D-073). Within one call the asset IDs must be strictly ascending and the
/// amounts greater than 0, at most [`crate::MAX_CALL_FUNDS`] items, otherwise the op fails (checked at execution).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Funds {
    /// The asset ID (KSA is 32 zero bytes).
    #[serde(with = "hex_array")]
    pub asset: [u8; 32],
    /// The amount.
    #[serde(with = "dec_u128")]
    pub amount: u128,
}

/// The execution rules for an attached funds list (D-073): at most [`crate::MAX_CALL_FUNDS`] items, asset IDs
/// strictly ascending (no duplicates), amounts greater than 0. A violation fails the op (not an invalid block).
pub fn check_funds(funds: &[Funds]) -> Result<(), &'static str> {
    if funds.len() > crate::MAX_CALL_FUNDS {
        return Err("attached funds exceed MAX_CALL_FUNDS items");
    }
    if funds.iter().any(|f| f.amount == 0) {
        return Err("attached fund amounts must be greater than 0");
    }
    if funds.windows(2).any(|w| match w {
        [a, b] => a.asset >= b.asset,
        _ => false,
    }) {
        return Err("attached fund asset IDs must be strictly ascending");
    }
    Ok(())
}

/// The signature scheme (SPEC §4.1, §4.2).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SigScheme {
    /// BIP340 Schnorr signature, used by the SDK and CLI.
    Bip340(#[serde(with = "hex_array")] [u8; 64]),
    /// BIP322 simple signature, used by browser wallets, implemented in M6.
    Bip322Simple(#[serde(with = "hex_bytes")] Vec<u8>),
}

/// Fast-lane block header v2 (SPEC §6.1, D-063). The Borsh encoding is `168 + 32 × parent count` bytes, with `nonce`
/// as the last 8 bytes.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockHeader {
    /// The block version ([`crate::BLOCK_VERSION`]).
    pub version: u32,
    /// The selected-chain depth: the height of the selected parent (`parents[0]`) plus 1, 0 for the genesis block
    /// (D-063).
    #[serde(with = "dec_u64")]
    pub height: u64,
    /// The parent hashes: `parents[0]` is the selected parent, the rest are strictly ascending in byte order; empty
    /// for the genesis block (D-063).
    #[serde(with = "hex_array_vec")]
    pub parents: Vec<[u8; 32]>,
    /// The latest BTC block seen when producing the block.
    pub btc_anchor: BtcAnchor,
    /// The Merkle root of all entries of this block.
    #[serde(with = "hex_array")]
    pub entries_root: [u8; 32],
    /// The state root after execution.
    #[serde(with = "hex_array")]
    pub state_root: [u8; 32],
    /// The producer's x-only public key, which receives the token fees.
    #[serde(with = "hex_array")]
    pub producer: [u8; 32],
    /// Seconds; only used for difficulty adjustment, never enters the VM.
    #[serde(with = "dec_u64")]
    pub timestamp: u64,
    /// The difficulty target.
    pub bits: u32,
    /// The PoW nonce.
    #[serde(with = "dec_u64")]
    pub nonce: u64,
}

impl BlockHeader {
    /// The Borsh encoding length without parents (genesis block): 4+8+4+(4+32)+32+32+32+8+4+8 = 168 bytes; plus 32
    /// bytes per parent.
    pub const BASE_ENCODED_LEN: usize = 168;

    /// The Borsh encoding length.
    pub fn encoded_len(&self) -> usize {
        self.parents
            .len()
            .saturating_mul(32)
            .saturating_add(Self::BASE_ENCODED_LEN)
    }

    /// The selected parent (`parents[0]`); `None` for the genesis block.
    pub fn selected_parent(&self) -> Option<&[u8; 32]> {
        self.parents.first()
    }

    /// The Borsh encoding, byte for byte identical to `borsh::to_vec(self)` but infallible
    /// (when the parent count exceeds `u32::MAX` the length prefix is written as `u32::MAX`; such a header can never
    /// be decoded or pass validation).
    pub fn to_bytes(&self) -> Vec<u8> {
        let count = u32::try_from(self.parents.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend(self.version.to_le_bytes());
        out.extend(self.height.to_le_bytes());
        out.extend(count.to_le_bytes());
        for parent in &self.parents {
            out.extend(parent);
        }
        out.extend(self.btc_anchor.height.to_le_bytes());
        out.extend(self.btc_anchor.hash);
        out.extend(self.entries_root);
        out.extend(self.state_root);
        out.extend(self.producer);
        out.extend(self.timestamp.to_le_bytes());
        out.extend(self.bits.to_le_bytes());
        out.extend(self.nonce.to_le_bytes());
        out
    }

    /// The block hash: `sha256d(borsh(header))` (SPEC §6.1).
    pub fn hash(&self) -> [u8; 32] {
        sha256d(&self.to_bytes())
    }
}

/// The BTC block a block is anchored to (SPEC §6.1).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtcAnchor {
    /// The BTC height.
    pub height: u32,
    /// The BTC block hash (internal byte order).
    #[serde(with = "hex_array")]
    pub hash: [u8; 32],
}

/// A fast-lane block (SPEC §6.1).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    /// The block header.
    pub header: BlockHeader,
    /// All entries.
    pub entries: Vec<Entry>,
    /// The signed settlement transactions (commit and reveal, D-081), see SPEC §7.
    pub settlement: Option<SettlementTxs>,
}

/// The settlement transactions carried by a block (D-081): the commit transaction and the reveal transaction, both in
/// Bitcoin network serialization. The settlement payload is in the witness of the reveal transaction (KSN1 envelope).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettlementTxs {
    /// The commit transaction: the producer's funding input, with the change and the P2TR output committing to the
    /// envelope as outputs.
    #[serde(with = "hex_bytes")]
    pub commit: Vec<u8>,
    /// The reveal transaction: spends the commitment output, carries the payload in the witness, with a P2A output and
    /// an empty OP_RETURN as outputs.
    #[serde(with = "hex_bytes")]
    pub reveal: Vec<u8>,
}

/// A block entry (SPEC §6.1).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Entry {
    /// Path C: a forcibly included native transaction.
    Forced {
        /// The BTC transaction ID carrying the VmOp (internal byte order).
        #[serde(with = "hex_array")]
        btc_txid: [u8; 32],
        /// The decoded VmOp.
        op: VmOp,
    },
    /// Path B: sponsored.
    Sponsored {
        /// The VmOp.
        op: VmOp,
    },
    /// Unlock token fees and block rewards, see SPEC §8.5, D-074, D-079: cumulatively unlocks in acceptance order up
    /// to and including this position.
    Unlock {
        /// The position unlocked up to: the height of the main-chain block that accepted it.
        #[serde(with = "dec_u64")]
        height: u64,
        /// The position unlocked up to: the index in that main-chain block's acceptance order (the merged blocks are
        /// 0, 1, … in turn, the main-chain block itself comes last).
        index: u32,
    },
}

/// The payload written into a BTC transaction (SPEC §7.1).
///
/// Encoding: the variant index (`u8`) followed by the fields. The header sequence in `Settlement` and `Headers` is
/// compressed per [`crate::compact`] (D-075, D-079), the bodies and entries per [`crate::compact_entries`] (D-080;
/// `Settlement` writes the bodies first, then this block's entries, sharing one key dictionary, with VmOps written
/// without chain ID); `NativeOp` is plain Borsh encoding. Encoding and decoding are [`Payload::encode`] /
/// [`Payload::decode`] (which need the network params). `bodies` (D-077) are checked for canonicality when decoding
/// ([`check_bodies`]).
// The variants are defined as in SPEC §7.1 (`NativeOp` embeds the VmOp directly), not boxed for memory layout.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Payload {
    /// The settlement transaction payload.
    Settlement {
        /// The headers, at most `MAX_SETTLEMENT_HEADERS`; the last one is this block's header, the rest are other
        /// headers not yet landed on BTC that the producer chose to carry (D-063).
        headers: Vec<BlockHeader>,
        /// All entries of this block; `Forced` entries only write `btc_txid`.
        entries: Vec<EntryData>,
        /// The carried bodies of other blocks (D-077); they cannot point at the last header.
        bodies: Vec<Body>,
    },
    /// The path C native transaction payload.
    NativeOp {
        /// The user's VmOp; `fee` must be `BtcNative`.
        op: VmOp,
    },
    /// Republishing of headers and bodies; anyone can send it.
    Headers {
        /// Any headers (self-certified by PoW, D-063), at most `MAX_SETTLEMENT_HEADERS`.
        headers: Vec<BlockHeader>,
        /// The bodies of some of those blocks (D-077, self-certified by `entries_root`).
        bodies: Vec<Body>,
        /// The payee account of the carrier reward (D-095): when this payload is the first to land some block's data
        /// on BTC and that block's data waited more than `ANCHOR_DEPTH + CARRIER_REPOST_DELAY` BTC blocks after its
        /// anchor, that block's carrier reward is paid to this account; fill in all zeros to decline (no account has
        /// that secret key).
        #[serde(with = "hex_array")]
        payee: [u8; 32],
    },
}

/// The body of one block carried by a payload (D-077): all entries (in payload form) of the block referred to by the
/// `header`-th header in the same payload.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    /// The index of the header in the same payload's header sequence.
    pub header: u32,
    /// All entries of that block; `Forced` entries only write `btc_txid`.
    pub entries: Vec<EntryData>,
}

/// Canonicality of `bodies` (D-077): indices strictly increasing, pointing at existing headers (a settlement payload
/// cannot point at the last header: this block's entries are in `entries`); a body has at least one entry other than
/// forced inclusions (a block with only forced-inclusion entries needs no body).
pub fn check_bodies(
    headers: &[BlockHeader],
    bodies: &[Body],
    settlement: bool,
) -> Result<(), String> {
    let mut previous: Option<u32> = None;
    for body in bodies {
        if previous.is_some_and(|p| body.header <= p) {
            return Err("body indices must be strictly increasing".into());
        }
        previous = Some(body.header);
        let index = usize::try_from(body.header).unwrap_or(usize::MAX);
        if settlement && index.checked_add(1) == Some(headers.len()) {
            return Err(
                "a body in a settlement payload cannot point at this block's own header".into(),
            );
        }
        if index >= headers.len() {
            return Err("body index exceeds the number of headers".into());
        }
        if body
            .entries
            .iter()
            .all(|e| matches!(e, EntryData::Forced { .. }))
        {
            return Err("a body must contain entries other than forced inclusions".into());
        }
    }
    Ok(())
}

fn invalid_bodies(message: String) -> borsh::io::Error {
    borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, message)
}

impl Payload {
    /// Write out the payload (see the type docs): `chain_id` is the network's chain ID, and the chain ID of VmOps in
    /// `Settlement` and `Headers` must equal it (not written in the payload, D-080).
    pub(crate) fn write<W: borsh::io::Write>(
        &self,
        chain_id: u32,
        writer: &mut W,
    ) -> borsh::io::Result<()> {
        match self {
            Payload::Settlement {
                headers,
                entries,
                bodies,
            } => {
                writer.write_all(&[0])?;
                crate::compact::write_headers(headers, writer)?;
                let mut encoder = crate::compact_entries::EntryEncoder::new(chain_id);
                writer.write_all(&encoder.encode_bodies(bodies)?)?;
                writer.write_all(&encoder.encode_entries(entries)?)
            }
            Payload::NativeOp { op } => {
                writer.write_all(&[1])?;
                BorshSerialize::serialize(op, writer)
            }
            Payload::Headers {
                headers,
                bodies,
                payee,
            } => {
                writer.write_all(&[2])?;
                crate::compact::write_headers(headers, writer)?;
                let mut encoder = crate::compact_entries::EntryEncoder::new(chain_id);
                writer.write_all(&encoder.encode_bodies(bodies)?)?;
                writer.write_all(payee)
            }
        }
    }

    /// Read a payload (see the type docs); the restored VmOps get chain ID `chain_id`, and a header sequence may hold at
    /// most `max_headers` headers (`MAX_SETTLEMENT_HEADERS`). The caller bounds the total length beforehand.
    pub(crate) fn read<R: borsh::io::Read>(
        reader: &mut R,
        chain_id: u32,
        max_headers: usize,
    ) -> borsh::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => {
                let headers = crate::compact::read_headers(reader, max_headers)?;
                let mut decoder = crate::compact_entries::EntryDecoder::new(chain_id);
                let bodies = decoder.bodies(reader)?;
                let entries = decoder.entries(reader)?;
                check_bodies(&headers, &bodies, true).map_err(invalid_bodies)?;
                Ok(Payload::Settlement {
                    headers,
                    entries,
                    bodies,
                })
            }
            1 => Ok(Payload::NativeOp {
                op: VmOp::deserialize_reader(reader)?,
            }),
            2 => {
                let headers = crate::compact::read_headers(reader, max_headers)?;
                let mut decoder = crate::compact_entries::EntryDecoder::new(chain_id);
                let bodies = decoder.bodies(reader)?;
                check_bodies(&headers, &bodies, false).map_err(invalid_bodies)?;
                let payee = <[u8; 32]>::deserialize_reader(reader)?;
                Ok(Payload::Headers {
                    headers,
                    bodies,
                    payee,
                })
            }
            tag => Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidData,
                format!("unknown payload type {tag}"),
            )),
        }
    }
}

/// An entry in a settlement payload (the `EntryData` of SPEC §7.1, not defined by the spec, see DECISIONS D-003).
///
/// Its variants correspond one to one to those of [`Entry`] with the same indices; except for `Forced`, which only
/// writes `btc_txid`, the fields and encoding of the other variants are exactly the same as [`Entry`].
// As for `Payload`: `Sponsored` embeds the VmOp directly, not boxed for memory layout.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum EntryData {
    /// Path C: only the BTC transaction ID is written; the indexer reads the VmOp from that transaction.
    Forced {
        /// The BTC transaction ID carrying the VmOp (internal byte order).
        #[serde(with = "hex_array")]
        btc_txid: [u8; 32],
    },
    /// Path B.
    Sponsored {
        /// The VmOp.
        op: VmOp,
    },
    /// Unlock token fees and block rewards (D-079: the acceptance position).
    Unlock {
        /// The height of the main-chain block that accepted it.
        #[serde(with = "dec_u64")]
        height: u64,
        /// The index in that main-chain block's acceptance order.
        index: u32,
    },
}

impl From<&Entry> for EntryData {
    fn from(entry: &Entry) -> Self {
        match entry {
            Entry::Forced { btc_txid, .. } => EntryData::Forced {
                btc_txid: *btc_txid,
            },
            Entry::Sponsored { op } => EntryData::Sponsored { op: op.clone() },
            Entry::Unlock { height, index } => EntryData::Unlock {
                height: *height,
                index: *index,
            },
        }
    }
}
