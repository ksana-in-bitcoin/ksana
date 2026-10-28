//! BTC data structures and Merkle proof verification (SPEC §9.4).
//!
//! All hashes are in **Bitcoin internal byte order** (the raw sha256d output, the byte order appearing in transaction
//! serialization; D-001); the hex shown by block explorers and RPC is its reverse. All parsing is bounds-checked,
//! returns `None` on a format error and never panics.
//!
//! Typical usage (in a contract):
//!
//! ```ignore
//! let header = btc::BlockHeader::parse(&ksana_contract_sdk::btc_header(height)).ok_or(..)?;
//! // coinbase_tx / coinbase_branch: the coinbase transaction of the same block and its branch, used to pin the
//! // Merkle tree depth
//! let txid = btc::verify_tx_inclusion(&header, &raw_tx, tx_index, &branch, &coinbase_tx, &coinbase_branch)
//!     .ok_or(..)?;
//! let outputs = btc::parse_outputs(&raw_tx).ok_or(..)?;
//! ```

use alloc::vec::Vec;

use crate::read::{take, take_array};
use crate::sha256d;

/// Block header length (bytes).
pub const HEADER_LEN: usize = 80;
/// Bitcoin's total supply cap (sats): neither a single output nor the sum of outputs may exceed it (consensus rule).
pub const MAX_MONEY: u64 = 21_000_000 * 100_000_000;
/// Upper bound of CompactSize (Bitcoin Core `MAX_SIZE`).
pub const MAX_COMPACT_SIZE: u64 = 0x0200_0000;
/// Maximum Merkle branch length (`tx_index` is u32).
pub const MAX_MERKLE_DEPTH: usize = 32;

/// 80-byte BTC block header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    /// Version.
    pub version: i32,
    /// Previous block hash (internal byte order).
    pub prev_blockhash: [u8; 32],
    /// Transaction Merkle root (internal byte order).
    pub merkle_root: [u8; 32],
    /// Timestamp.
    pub time: u32,
    /// Difficulty target (compact format).
    pub bits: u32,
    /// nonce.
    pub nonce: u32,
}

impl BlockHeader {
    /// Parses a block header; the length must be exactly 80 bytes.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let mut input = bytes;
        let header = Self {
            version: i32::from_le_bytes(take_array(&mut input)?),
            prev_blockhash: take_array(&mut input)?,
            merkle_root: take_array(&mut input)?,
            time: u32::from_le_bytes(take_array(&mut input)?),
            bits: u32::from_le_bytes(take_array(&mut input)?),
            nonce: u32::from_le_bytes(take_array(&mut input)?),
        };
        input.is_empty().then_some(header)
    }

    /// Serializes to 80 bytes.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        let fields = self
            .version
            .to_le_bytes()
            .into_iter()
            .chain(self.prev_blockhash)
            .chain(self.merkle_root)
            .chain(self.time.to_le_bytes())
            .chain(self.bits.to_le_bytes())
            .chain(self.nonce.to_le_bytes());
        for (dst, src) in out.iter_mut().zip(fields) {
            *dst = src;
        }
        out
    }

    /// Block hash: `sha256d(header)`, internal byte order.
    pub fn block_hash(&self) -> [u8; 32] {
        sha256d(&self.to_bytes())
    }
}

/// Transaction ID: `sha256d(raw_tx)`, internal byte order.
///
/// `raw_tx` must be the serialization **without witness data** (for a segwit transaction, strip the marker, flag and
/// witness first); computed over a serialization with witness it gives the wtxid. [`parse_outputs`] rejects a
/// serialization with witness.
pub fn txid(raw_tx: &[u8]) -> [u8; 32] {
    sha256d(raw_tx)
}

/// Transaction output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxOut<'a> {
    /// Value (sats).
    pub value: u64,
    /// Locking script.
    pub script_pubkey: &'a [u8],
}

/// Reads a Bitcoin CompactSize, requiring the minimal encoding and at most [`MAX_COMPACT_SIZE`] (consistent with
/// Bitcoin Core's `ReadCompactSize`).
fn compact_size(input: &mut &[u8]) -> Option<u64> {
    let [first] = take_array::<1>(input)?;
    let (value, min) = match first {
        0xfd => (u64::from(u16::from_le_bytes(take_array(input)?)), 0xfd),
        0xfe => (u64::from(u32::from_le_bytes(take_array(input)?)), 0x1_0000),
        0xff => (u64::from_le_bytes(take_array(input)?), 0x1_0000_0000),
        small => (u64::from(small), 0),
    };
    (value >= min && value <= MAX_COMPACT_SIZE).then_some(value)
}

fn compact_len(input: &mut &[u8]) -> Option<usize> {
    usize::try_from(compact_size(input)?).ok()
}

/// Parses a transaction serialization **without witness data** and returns all outputs.
///
/// Fully parses the version, inputs (at least 1), outputs (at least 1) and lock time, requiring exactly all bytes to be
/// consumed. Returns `None` in these cases: truncated or trailing bytes, an input count of 0 (including the marker
/// `0x00` of a segwit serialization), an output count of 0, a non-minimal CompactSize, a single output or the sum of
/// outputs exceeding [`MAX_MONEY`].
pub fn parse_outputs(raw_tx: &[u8]) -> Option<Vec<TxOut<'_>>> {
    let mut input = raw_tx;
    take(&mut input, 4)?; // version
    let input_count = compact_size(&mut input)?;
    if input_count == 0 {
        return None;
    }
    for _ in 0..input_count {
        take(&mut input, 36)?; // previous outpoint: txid + vout
        let script_len = compact_len(&mut input)?;
        take(&mut input, script_len)?; // scriptSig
        take(&mut input, 4)?; // sequence
    }
    let output_count = compact_size(&mut input)?;
    if output_count == 0 {
        return None;
    }
    let mut outputs = Vec::new();
    let mut total: u64 = 0;
    for _ in 0..output_count {
        let value = u64::from_le_bytes(take_array(&mut input)?);
        total = total.checked_add(value)?;
        if value > MAX_MONEY || total > MAX_MONEY {
            return None;
        }
        let script_len = compact_len(&mut input)?;
        let script_pubkey = take(&mut input, script_len)?;
        outputs.push(TxOut {
            value,
            script_pubkey,
        });
    }
    take(&mut input, 4)?; // lock time
    input.is_empty().then_some(outputs)
}

fn concat64(left: &[u8; 32], right: &[u8; 32]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for (dst, src) in out.iter_mut().zip(left.iter().chain(right.iter())) {
        *dst = *src;
    }
    out
}

/// Computes the Merkle root from the leaf (transaction ID), position `index` and the Merkle branch.
///
/// Level i: when bit i of `index` is 1 the current node is the right child (`sha256d(sibling ‖ current)`), otherwise
/// the left child (`sha256d(current ‖ sibling)`). Returns `None` when the branch is longer than [`MAX_MERKLE_DEPTH`]
/// or `index` has non-zero bits beyond the branch length (only one position representation is accepted for the same
/// transaction).
pub fn merkle_root_from_branch(
    leaf: &[u8; 32],
    index: u32,
    branch: &[[u8; 32]],
) -> Option<[u8; 32]> {
    if branch.len() > MAX_MERKLE_DEPTH {
        return None;
    }
    let depth = u32::try_from(branch.len()).ok()?;
    if index.checked_shr(depth).unwrap_or(0) != 0 {
        return None;
    }
    let mut node = *leaf;
    let mut position = index;
    for sibling in branch {
        node = if position & 1 == 1 {
            sha256d(&concat64(sibling, &node))
        } else {
            sha256d(&concat64(&node, sibling))
        };
        position >>= 1;
    }
    Some(node)
}

/// Verifies a Merkle branch: the root computed from `leaf`, `index`, `branch` equals `root`.
pub fn verify_merkle_branch(
    leaf: &[u8; 32],
    index: u32,
    branch: &[[u8; 32]],
    root: &[u8; 32],
) -> bool {
    merkle_root_from_branch(leaf, index, branch).is_some_and(|computed| computed == *root)
}

/// Whether `raw_tx` (serialization without witness) is a coinbase transaction: exactly one input whose referenced
/// previous output is null (all-zero transaction ID, index `0xffffffff`). A serialization with witness (the `0x00`
/// marker after the version) returns `false`.
pub fn is_coinbase(raw_tx: &[u8]) -> bool {
    let mut input = raw_tx;
    let prevout = take(&mut input, 4) // version
        .and_then(|_| compact_size(&mut input))
        .filter(|count| *count == 1)
        .and_then(|_| take_array::<36>(&mut input));
    prevout.is_some_and(|p| {
        p.iter().take(32).all(|b| *b == 0) && p.iter().skip(32).all(|b| *b == 0xff)
    })
}

/// Verifies that `raw_tx` (serialization without witness) is transaction number `index` of block `header`; on success
/// returns its transaction ID.
///
/// The caller must also provide the same block's coinbase transaction `coinbase_tx` (serialization without witness)
/// and its Merkle branch `coinbase_branch` (position 0), used to pin the depth of the Merkle tree. Bitcoin's Merkle
/// tree duplicates the last node on odd levels, so all transactions are at the same depth; the preimage of an internal
/// node is exactly 64 bytes (two 32-byte hashes), hence two forgeries (CVE-2017-12842):
///
/// - passing an internal node off as a transaction (a proof shallower than the real leaves): a `raw_tx` of exactly
///   64 bytes is always rejected;
/// - treating a real 64-byte transaction in the block as an internal node and grafting a forged transaction below it
///   (a proof one level deeper than the real leaves; the forged transaction itself is not 64 bytes): the transaction's
///   branch is required to have the same length as the coinbase's. The coinbase is always at position 0 and is a real
///   leaf, so it determines the depth.
///
/// The coinbase must satisfy [`is_coinbase`].
///
/// `index` does not identify the transaction uniquely (CVE-2012-2459): because the last node of an odd level is paired
/// with itself, a transaction at the end of the block can also verify at a higher index that holds no transaction (for
/// example the last of 3 transactions at index 3 as well as 2). Key replay protection on the returned transaction ID (or
/// on its outputs), never on `index`.
pub fn verify_tx_inclusion(
    header: &BlockHeader,
    raw_tx: &[u8],
    index: u32,
    branch: &[[u8; 32]],
    coinbase_tx: &[u8],
    coinbase_branch: &[[u8; 32]],
) -> Option<[u8; 32]> {
    if raw_tx.len() == 64 || branch.len() != coinbase_branch.len() || !is_coinbase(coinbase_tx) {
        return None;
    }
    if !verify_merkle_branch(&txid(coinbase_tx), 0, coinbase_branch, &header.merkle_root) {
        return None;
    }
    let id = txid(raw_tx);
    verify_merkle_branch(&id, index, branch, &header.merkle_root).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_size_rules() {
        let parse = |bytes: &[u8]| {
            let mut input = bytes;
            compact_size(&mut input).filter(|_| input.is_empty())
        };
        assert_eq!(parse(&[0x00]), Some(0));
        assert_eq!(parse(&[0xfc]), Some(0xfc));
        assert_eq!(parse(&[0xfd, 0xfd, 0x00]), Some(0xfd));
        assert_eq!(parse(&[0xfd, 0xfc, 0x00]), None, "non-canonical");
        assert_eq!(parse(&[0xfe, 0x00, 0x00, 0x01, 0x00]), Some(0x1_0000));
        assert_eq!(
            parse(&[0xfe, 0xff, 0xff, 0x00, 0x00]),
            None,
            "non-canonical"
        );
        assert_eq!(
            parse(&[0xfe, 0x00, 0x00, 0x00, 0x02]),
            Some(MAX_COMPACT_SIZE)
        );
        assert_eq!(
            parse(&[0xfe, 0x01, 0x00, 0x00, 0x02]),
            None,
            "above MAX_SIZE"
        );
        assert_eq!(
            parse(&[0xff, 0, 0, 0, 0, 1, 0, 0, 0]),
            None,
            "above MAX_SIZE"
        );
        assert_eq!(parse(&[0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]), None);
        assert_eq!(parse(&[0xfd, 0xfd]), None, "truncated");
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn merkle_index_must_fit_branch() {
        let leaf = [1u8; 32];
        assert_eq!(merkle_root_from_branch(&leaf, 0, &[]), Some(leaf));
        assert_eq!(merkle_root_from_branch(&leaf, 1, &[]), None);
        assert!(merkle_root_from_branch(&leaf, 3, &[[0u8; 32]; 2]).is_some());
        assert_eq!(merkle_root_from_branch(&leaf, 4, &[[0u8; 32]; 2]), None);
        assert!(merkle_root_from_branch(&leaf, u32::MAX, &[[0u8; 32]; 32]).is_some());
        assert_eq!(merkle_root_from_branch(&leaf, 0, &[[0u8; 32]; 33]), None);
    }

    fn node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        sha256d(&concat64(left, right))
    }

    /// A transaction without witness: one input (referencing `prevout`), one output.
    fn simple_tx(prevout: [u8; 36], script_sig: &[u8], script_pubkey: &[u8]) -> Vec<u8> {
        let mut tx = 2u32.to_le_bytes().to_vec();
        tx.push(1);
        tx.extend_from_slice(&prevout);
        tx.push(u8::try_from(script_sig.len()).unwrap());
        tx.extend_from_slice(script_sig);
        tx.extend_from_slice(&u32::MAX.to_le_bytes());
        tx.push(1);
        tx.extend_from_slice(&5_000u64.to_le_bytes());
        tx.push(u8::try_from(script_pubkey.len()).unwrap());
        tx.extend_from_slice(script_pubkey);
        tx.extend_from_slice(&0u32.to_le_bytes());
        tx
    }

    fn coinbase() -> Vec<u8> {
        let mut prevout = [0u8; 36];
        prevout[32..].copy_from_slice(&[0xff; 4]);
        simple_tx(prevout, &[0x03, 0x01, 0x02, 0x03], &[0x51])
    }

    fn header_with_root(merkle_root: [u8; 32]) -> BlockHeader {
        BlockHeader {
            version: 0x2000_0000,
            prev_blockhash: [0x22; 32],
            merkle_root,
            time: 1_700_000_000,
            bits: 0x207f_ffff,
            nonce: 0,
        }
    }

    #[test]
    fn coinbase_detection() {
        assert!(is_coinbase(&coinbase()));
        assert!(!is_coinbase(&simple_tx([0x11; 36], &[], &[0x51])));
        // The previous output's index is not 0xffffffff
        let mut prevout = [0u8; 36];
        prevout[32..].copy_from_slice(&[0xff, 0xff, 0xff, 0xfe]);
        assert!(!is_coinbase(&simple_tx(prevout, &[0x51, 0x51], &[0x51])));
        // A serialization with witness (the 0x00 marker after the version) and truncated data
        let mut witness = coinbase();
        witness.splice(4..4, [0x00, 0x01]);
        assert!(!is_coinbase(&witness));
        assert!(!is_coinbase(&coinbase()[..20]));
        assert!(!is_coinbase(&[]));
    }

    /// The block has a real 64-byte transaction `T = L ‖ txid(F)`: treating T as an internal node, the forged
    /// transaction F gets a branch one level deeper than the real leaves that computes to the same Merkle root.
    /// Rejecting only 64-byte `raw_tx` does not stop it (F is not 64 bytes); the depth pinned by the coinbase is
    /// needed.
    #[test]
    fn a_fake_transaction_grafted_below_a_64_byte_transaction_is_rejected() {
        let fake = simple_tx([0x33; 36], &[], &[0x00, 0x14, 0x55, 0x55]);
        let left = [0x77u8; 32];
        let mut t = [0u8; 64];
        t[..32].copy_from_slice(&left);
        t[32..].copy_from_slice(&txid(&fake));
        let other = simple_tx([0x44; 36], &[], &[0x51]);
        let cb = coinbase();
        let (cb_id, t_id, other_id) = (txid(&cb), txid(&t), txid(&other));
        // 3 transactions [coinbase, T, other], depth 2 (the last node is duplicated)
        let upper_right = node(&other_id, &other_id);
        let root = node(&node(&cb_id, &t_id), &upper_right);
        let header = header_with_root(root);
        let cb_branch = [t_id, upper_right];

        // The real transaction passes as usual
        let other_branch = [other_id, node(&cb_id, &t_id)];
        assert_eq!(
            verify_tx_inclusion(&header, &other, 2, &other_branch, &cb, &cb_branch),
            Some(other_id)
        );
        // The grafted branch does compute to the same root (this is the vulnerability)...
        let grafted = [left, cb_id, upper_right];
        assert!(verify_merkle_branch(&txid(&fake), 3, &grafted, &root));
        // ...but it is one level deeper than the coinbase's branch and is rejected
        assert_eq!(
            verify_tx_inclusion(&header, &fake, 3, &grafted, &cb, &cb_branch),
            None
        );
        // The 64-byte T itself cannot be proven either
        assert_eq!(
            verify_tx_inclusion(&header, &t, 1, &cb_branch, &cb, &cb_branch),
            None
        );
    }

    #[test]
    fn the_coinbase_proof_must_be_genuine() {
        let cb = coinbase();
        let tx = simple_tx([0x44; 36], &[], &[0x51]);
        let (cb_id, tx_id) = (txid(&cb), txid(&tx));
        let header = header_with_root(node(&cb_id, &tx_id));
        assert_eq!(
            verify_tx_inclusion(&header, &tx, 1, &[cb_id], &cb, &[tx_id]),
            Some(tx_id)
        );
        // The coinbase itself can be proven too (position 0)
        assert_eq!(
            verify_tx_inclusion(&header, &cb, 0, &[tx_id], &cb, &[tx_id]),
            Some(cb_id)
        );
        // A non-coinbase transaction at position 0, a wrong branch and unequal lengths are all rejected
        assert_eq!(
            verify_tx_inclusion(&header, &tx, 1, &[cb_id], &tx, &[cb_id]),
            None
        );
        assert_eq!(
            verify_tx_inclusion(&header, &tx, 1, &[cb_id], &cb, &[[0u8; 32]]),
            None
        );
        assert_eq!(
            verify_tx_inclusion(&header, &tx, 1, &[cb_id], &cb, &[tx_id, tx_id]),
            None
        );
    }
}
