//! Compressed encoding of the header sequence in a payload (D-075, D-079, revising SPEC §7.1): each header in a
//! `Settlement` or `Headers` payload is encoded relative to the headers before it in the sequence, and decoding
//! restores the full headers (the headers themselves and the block hashes are unchanged).
//!
//! Sequence = `u32` count (little-endian, like Borsh's `Vec` prefix), then each header:
//!
//! | Field | Encoding |
//! | --- | --- |
//! | `flags: u8` | bit 0 version equal to the previous one, bit 1 BTC anchor equal to the previous one, bit 2 `bits` equal to the previous one, bit 3 `entries_root` is the root of the empty entry list, bit 4 `state_root` equal to the previous one (D-079); the other bits are 0 |
//! | version | `u32` when bit 0 is not set |
//! | height | varint for the first one, afterwards the zigzag varint of the delta from the previous one (modulo 2^64, interpreted as `i64`) |
//! | parents | varint count, each parent a varint `r`: `r ≥ 1` refers to the hash of the `r`-th header before it in the sequence, `r = 0` is followed by the 32-byte hash |
//! | BTC anchor | `u32` height + 32-byte hash when bit 1 is not set |
//! | `entries_root` | 32 bytes when bit 3 is not set |
//! | `state_root` | 32 bytes when bit 4 is not set |
//! | producer | varint `r`: `r ≥ 1` is the producer of the `r`-th header before it, `r = 0` is followed by the 32 bytes |
//! | timestamp | same as height |
//! | `bits` | `u32` when bit 2 is not set |
//! | `nonce` | `u64` |
//!
//! varints are LEB128 (unsigned, minimal encoding). The encoding is canonical and the decoder checks: a field equal to
//! the previous one must use its flag, the first header sets neither bits 0-2 nor bit 4, the empty root must use
//! bit 3; a parent already seen in the sequence must be referenced (the most recent one), and the producer likewise
//! references the most recent equal producer; varints must be minimally encoded. Hence
//! `read_headers(write_headers(h)) == h`, and for any accepted input re-encoding yields the same bytes.
//!
//! The version, anchor, `bits` and `state_root` of the first header are always written out (there is no previous one
//! to compare with). Height and timestamp deltas are modulo 2^64, so any value can be encoded.
//!
//! An empty block (neither the main-chain block nor the blocks it merges have entries) does not change the state
//! (D-079), and its state root equals the selected parent's: in a run of empty blocks only the first header writes
//! the state root, each later one is 14 bytes (flags 1, height 1, parents 2, producer 1, timestamp 1, `nonce` 8).

use std::io::{Error, ErrorKind, Read, Result, Write};

use crate::hash::sha256;
use crate::types::{BlockHeader, BtcAnchor};

const SAME_VERSION: u8 = 1;
const SAME_ANCHOR: u8 = 1 << 1;
const SAME_BITS: u8 = 1 << 2;
const EMPTY_ENTRIES: u8 = 1 << 3;
const SAME_STATE_ROOT: u8 = 1 << 4;
const KNOWN_FLAGS: u8 = SAME_VERSION | SAME_ANCHOR | SAME_BITS | EMPTY_ENTRIES | SAME_STATE_ROOT;
/// Flags the first header cannot set (there is no previous one to compare with).
const SAME_AS_PREVIOUS: u8 = SAME_VERSION | SAME_ANCHOR | SAME_BITS | SAME_STATE_ROOT;
/// The maximum number of parents of a header (codec limit, far above `MAX_BLOCK_PARENTS`, only to bound allocation).
const MAX_PARENTS: u64 = 255;
/// The maximum number of headers preallocated when decoding (the count prefix is untrusted).
const PREALLOCATE: usize = 64;

/// The `entries_root` of the empty entry list (RFC 6962's empty tree: `sha256("")`).
pub fn empty_entries_root() -> [u8; 32] {
    sha256(&[])
}

pub(crate) fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::InvalidData, message.to_string())
}

pub(crate) fn write_varint<W: Write>(writer: &mut W, mut value: u64) -> Result<()> {
    loop {
        let low = u8::try_from(value & 0x7f).map_err(|_| invalid("varint encoding error"))?;
        value >>= 7;
        if value == 0 {
            return writer.write_all(&[low]);
        }
        writer.write_all(&[low | 0x80])?;
    }
}

pub(crate) fn read_byte<R: Read>(reader: &mut R) -> Result<u8> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

/// LEB128, required to be minimally encoded and not to exceed `u64`.
pub(crate) fn read_varint<R: Read>(reader: &mut R) -> Result<u64> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = read_byte(reader)?;
        let low = u64::from(byte & 0x7f);
        if shift == 63 && low > 1 {
            return Err(invalid("varint exceeds u64"));
        }
        value |= low
            .checked_shl(shift)
            .ok_or_else(|| invalid("varint too long"))?;
        if byte & 0x80 == 0 {
            if byte == 0 && shift > 0 {
                return Err(invalid("varint is not minimally encoded"));
            }
            return Ok(value);
        }
        shift = shift
            .checked_add(7)
            .filter(|s| *s <= 63)
            .ok_or_else(|| invalid("varint too long"))?;
    }
}

fn zigzag(value: i64) -> u64 {
    // (value << 1) ^ (value >> 63), reinterpreted bitwise
    let shifted = value.wrapping_shl(1) ^ value.wrapping_shr(63);
    u64::from_le_bytes(shifted.to_le_bytes())
}

fn unzigzag(value: u64) -> i64 {
    let half = i64::from_le_bytes((value >> 1).to_le_bytes());
    let sign = i64::from_le_bytes((value & 1).to_le_bytes());
    half ^ sign.wrapping_neg()
}

/// The zigzag varint of the delta from the previous value (modulo 2^64, interpreted as `i64`): any `u64` can be
/// encoded, and nearby values take only one or two bytes.
fn write_delta<W: Write>(writer: &mut W, value: u64, previous: u64) -> Result<()> {
    let delta = i64::from_le_bytes(value.wrapping_sub(previous).to_le_bytes());
    write_varint(writer, zigzag(delta))
}

fn read_delta<R: Read>(reader: &mut R, previous: u64) -> Result<u64> {
    let delta = unzigzag(read_varint(reader)?);
    Ok(previous.wrapping_add(u64::from_le_bytes(delta.to_le_bytes())))
}

pub(crate) fn read_array<R: Read>(reader: &mut R) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    reader.read_exact(&mut out)?;
    Ok(out)
}

pub(crate) fn read_u32<R: Read>(reader: &mut R) -> Result<u32> {
    let mut out = [0u8; 4];
    reader.read_exact(&mut out)?;
    Ok(u32::from_le_bytes(out))
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64> {
    let mut out = [0u8; 8];
    reader.read_exact(&mut out)?;
    Ok(u64::from_le_bytes(out))
}

/// The distance (`r ≥ 1`) from `index` to the nearest position before it in the sequence that satisfies `matches`.
fn back_reference<T>(items: &[T], index: usize, matches: impl Fn(&T) -> bool) -> Option<u64> {
    let before = items.get(..index)?;
    let position = before.iter().rposition(matches)?;
    index
        .checked_sub(position)
        .and_then(|r| u64::try_from(r).ok())
}

/// The key of the `r ≥ 1`-th element before `index` in the sequence, checking that it is the element with that key
/// nearest to `index` (canonical encoding).
fn reference<T, K: PartialEq>(
    items: &[T],
    index: usize,
    r: u64,
    key: impl Fn(&T) -> K,
) -> Result<K> {
    let distance = usize::try_from(r).map_err(|_| invalid("reference out of range"))?;
    let position = index
        .checked_sub(distance)
        .ok_or_else(|| invalid("reference out of range"))?;
    let target = key(items
        .get(position)
        .ok_or_else(|| invalid("reference out of range"))?);
    let closer = position
        .checked_add(1)
        .and_then(|from| items.get(from..index))
        .ok_or_else(|| invalid("reference out of range"))?;
    if closer.iter().any(|item| key(item) == target) {
        return Err(invalid(
            "reference is not the most recent equal element (non-canonical encoding)",
        ));
    }
    Ok(target)
}

/// Header-by-header encoding of a header sequence: each header relative to the headers encoded before it (see the
/// module docs). Used to fit headers one by one within the payload limit.
#[derive(Default)]
pub struct HeaderEncoder {
    previous: Option<BlockHeader>,
    hashes: Vec<[u8; 32]>,
    producers: Vec<[u8; 32]>,
}

impl HeaderEncoder {
    /// The encoding of the next header in the sequence (without the count at the start of the sequence).
    pub fn encode(&mut self, header: &BlockHeader) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let previous = self.previous.as_ref();
        let mut flags = 0u8;
        if previous.is_some_and(|p| p.version == header.version) {
            flags |= SAME_VERSION;
        }
        if previous.is_some_and(|p| p.btc_anchor == header.btc_anchor) {
            flags |= SAME_ANCHOR;
        }
        if previous.is_some_and(|p| p.bits == header.bits) {
            flags |= SAME_BITS;
        }
        if header.entries_root == empty_entries_root() {
            flags |= EMPTY_ENTRIES;
        }
        if previous.is_some_and(|p| p.state_root == header.state_root) {
            flags |= SAME_STATE_ROOT;
        }
        out.write_all(&[flags])?;
        if flags & SAME_VERSION == 0 {
            out.write_all(&header.version.to_le_bytes())?;
        }
        match previous {
            None => write_varint(&mut out, header.height)?,
            Some(p) => write_delta(&mut out, header.height, p.height)?,
        }
        let parents = u64::try_from(header.parents.len())
            .ok()
            .filter(|n| *n <= MAX_PARENTS)
            .ok_or_else(|| invalid("too many parents"))?;
        write_varint(&mut out, parents)?;
        let index = self.hashes.len();
        for parent in &header.parents {
            match back_reference(&self.hashes, index, |h| h == parent) {
                Some(r) => write_varint(&mut out, r)?,
                None => {
                    write_varint(&mut out, 0)?;
                    out.write_all(parent)?;
                }
            }
        }
        if flags & SAME_ANCHOR == 0 {
            out.write_all(&header.btc_anchor.height.to_le_bytes())?;
            out.write_all(&header.btc_anchor.hash)?;
        }
        if flags & EMPTY_ENTRIES == 0 {
            out.write_all(&header.entries_root)?;
        }
        if flags & SAME_STATE_ROOT == 0 {
            out.write_all(&header.state_root)?;
        }
        match back_reference(&self.producers, index, |p| *p == header.producer) {
            Some(r) => write_varint(&mut out, r)?,
            None => {
                write_varint(&mut out, 0)?;
                out.write_all(&header.producer)?;
            }
        }
        match previous {
            None => write_varint(&mut out, header.timestamp)?,
            Some(p) => write_delta(&mut out, header.timestamp, p.timestamp)?,
        }
        if flags & SAME_BITS == 0 {
            out.write_all(&header.bits.to_le_bytes())?;
        }
        out.write_all(&header.nonce.to_le_bytes())?;
        self.hashes.push(header.hash());
        self.producers.push(header.producer);
        self.previous = Some(header.clone());
        Ok(out)
    }
}

/// Write out a header sequence (see the module docs).
pub fn write_headers<W: Write>(headers: &[BlockHeader], writer: &mut W) -> Result<()> {
    let count = u32::try_from(headers.len()).map_err(|_| invalid("too many headers"))?;
    writer.write_all(&count.to_le_bytes())?;
    let mut encoder = HeaderEncoder::default();
    for header in headers {
        writer.write_all(&encoder.encode(header)?)?;
    }
    Ok(())
}

/// The encoded length of a header sequence (including the 4-byte count at the start).
pub fn encoded_len<'a>(headers: impl IntoIterator<Item = &'a BlockHeader>) -> Result<usize> {
    let mut encoder = HeaderEncoder::default();
    let mut len: usize = 4;
    for header in headers {
        len = len.saturating_add(encoder.encode(header)?.len());
    }
    Ok(len)
}

/// Read a header sequence of at most `max_count` headers (a payload carries at most `MAX_SETTLEMENT_HEADERS`, checked
/// on the count before any header is read) and check that the encoding is canonical (see the module docs). The caller
/// bounds the total length beforehand (a payload does not exceed `MAX_BLOCK_DATA_BYTES`).
pub fn read_headers<R: Read>(reader: &mut R, max_count: usize) -> Result<Vec<BlockHeader>> {
    let count = read_u32(reader)?;
    let count = usize::try_from(count)
        .ok()
        .filter(|count| *count <= max_count)
        .ok_or_else(|| invalid("too many headers"))?;
    let empty = empty_entries_root();
    let mut headers: Vec<BlockHeader> = Vec::with_capacity(count.min(PREALLOCATE));
    let mut hashes: Vec<[u8; 32]> = Vec::with_capacity(count.min(PREALLOCATE));
    for index in 0..count {
        let previous = headers.last();
        let flags = read_byte(reader)?;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(invalid("unknown flag bit"));
        }
        if previous.is_none() && flags & SAME_AS_PREVIOUS != 0 {
            return Err(invalid("the first header cannot refer to a previous one"));
        }
        let version = match previous {
            Some(p) if flags & SAME_VERSION != 0 => p.version,
            _ => {
                let version = read_u32(reader)?;
                if previous.is_some_and(|p| p.version == version) {
                    return Err(invalid("version equal to the previous one must use the flag (non-canonical encoding)"));
                }
                version
            }
        };
        let height = match previous {
            None => read_varint(reader)?,
            Some(p) => read_delta(reader, p.height)?,
        };
        let parent_count = read_varint(reader)?;
        if parent_count > MAX_PARENTS {
            return Err(invalid("too many parents"));
        }
        let mut parents = Vec::new();
        for _ in 0..parent_count {
            let r = read_varint(reader)?;
            let parent = if r == 0 {
                let parent = read_array(reader)?;
                if hashes.contains(&parent) {
                    return Err(invalid(
                        "a parent within the sequence must be referenced (non-canonical encoding)",
                    ));
                }
                parent
            } else {
                reference(&hashes, index, r, |h| *h)?
            };
            parents.push(parent);
        }
        let btc_anchor = match previous {
            Some(p) if flags & SAME_ANCHOR != 0 => p.btc_anchor.clone(),
            _ => {
                let anchor = BtcAnchor {
                    height: read_u32(reader)?,
                    hash: read_array(reader)?,
                };
                if previous.is_some_and(|p| p.btc_anchor == anchor) {
                    return Err(invalid("anchor equal to the previous one must use the flag (non-canonical encoding)"));
                }
                anchor
            }
        };
        let entries_root = if flags & EMPTY_ENTRIES != 0 {
            empty
        } else {
            let root = read_array(reader)?;
            if root == empty {
                return Err(invalid(
                    "the root of an empty entry list must use the flag (non-canonical encoding)",
                ));
            }
            root
        };
        let state_root = match previous {
            Some(p) if flags & SAME_STATE_ROOT != 0 => p.state_root,
            _ => {
                let root = read_array(reader)?;
                if previous.is_some_and(|p| p.state_root == root) {
                    return Err(invalid("state root equal to the previous one must use the flag (non-canonical encoding)"));
                }
                root
            }
        };
        let r = read_varint(reader)?;
        let producer = if r == 0 {
            let producer = read_array(reader)?;
            if headers.iter().any(|h| h.producer == producer) {
                return Err(invalid("a producer already seen in the sequence must be referenced (non-canonical encoding)"));
            }
            producer
        } else {
            reference(&headers, index, r, |h| h.producer)?
        };
        let timestamp = match previous {
            None => read_varint(reader)?,
            Some(p) => read_delta(reader, p.timestamp)?,
        };
        let bits = match previous {
            Some(p) if flags & SAME_BITS != 0 => p.bits,
            _ => {
                let bits = read_u32(reader)?;
                if previous.is_some_and(|p| p.bits == bits) {
                    return Err(invalid(
                        "bits equal to the previous one must use the flag (non-canonical encoding)",
                    ));
                }
                bits
            }
        };
        let nonce = read_u64(reader)?;
        let header = BlockHeader {
            version,
            height,
            parents,
            btc_anchor,
            entries_root,
            state_root,
            producer,
            timestamp,
            bits,
            nonce,
        };
        hashes.push(header.hash());
        headers.push(header);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(height: u64, parents: Vec<[u8; 32]>, producer: u8) -> BlockHeader {
        BlockHeader {
            version: 2,
            height,
            parents,
            btc_anchor: BtcAnchor {
                height: 100,
                hash: [7; 32],
            },
            entries_root: empty_entries_root(),
            state_root: [u8::try_from(height % 256).unwrap(); 32],
            producer: [producer; 32],
            timestamp: 1_700_000_000 + height,
            bits: 0x207f_ffff,
            nonce: height.wrapping_mul(0x9e37_79b9),
        }
    }

    fn encode(headers: &[BlockHeader]) -> Vec<u8> {
        let mut out = Vec::new();
        write_headers(headers, &mut out).unwrap();
        out
    }

    fn decode(bytes: &[u8]) -> Result<Vec<BlockHeader>> {
        let mut reader = bytes;
        let headers = read_headers(&mut reader, usize::MAX)?;
        if !reader.is_empty() {
            return Err(invalid("trailing bytes"));
        }
        Ok(headers)
    }

    /// A chain: each header's parent is the previous one; the first header's parent is not in the sequence.
    fn chain(n: u64) -> Vec<BlockHeader> {
        let mut headers: Vec<BlockHeader> = Vec::new();
        for i in 0..n {
            let parent = headers.last().map_or([1; 32], |h| h.hash());
            headers.push(header(10 + i, vec![parent], 5));
        }
        headers
    }

    #[test]
    fn varints_are_minimal_and_bounded() {
        for value in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut out = Vec::new();
            write_varint(&mut out, value).unwrap();
            assert_eq!(read_varint(&mut out.as_slice()).unwrap(), value);
        }
        // Non-minimal encoding and exceeding u64
        assert!(read_varint(&mut [0x80, 0x00].as_slice()).is_err());
        assert!(read_varint(&mut [0xff; 10].as_slice()).is_err());
        let mut max = vec![0xff; 9];
        max.push(0x01);
        assert_eq!(read_varint(&mut max.as_slice()).unwrap(), u64::MAX);
        let mut over = vec![0xff; 9];
        over.push(0x02);
        assert!(read_varint(&mut over.as_slice()).is_err());
        for value in [0i64, 1, -1, 63, -64, i64::MAX, i64::MIN] {
            assert_eq!(unzigzag(zigzag(value)), value);
        }
    }

    #[test]
    fn headers_with_changing_state_roots_compress_to_46_bytes_each() {
        let headers = chain(100);
        let bytes = encode(&headers);
        assert_eq!(decode(&bytes).unwrap(), headers);
        assert_eq!(encoded_len(&headers).unwrap(), bytes.len());
        let full: usize = headers.iter().map(BlockHeader::encoded_len).sum();
        // The first header writes out all fields; each later one: flags 1 + height 1 + parents 2 + state_root 32 +
        // producer 1 + timestamp 1 + nonce 8
        assert_eq!(
            bytes.len(),
            4 + (1 + 4 + 1 + 1 + 1 + 32 + 36 + 32 + 1 + 32 + 5 + 4 + 8) + 99 * 46
        );
        assert!(bytes.len() * 4 < full, "{} vs {full}", bytes.len());
    }

    #[test]
    fn dag_parents_producers_and_changed_fields_roundtrip() {
        let mut headers = chain(3);
        let (a, b) = (headers[1].hash(), headers[2].hash());
        // Merge two blocks within the sequence and one outside it; another producer; anchor, bits, version and
        // entries root change
        let mut merge = header(13, vec![b, a, [9; 32]], 6);
        merge.btc_anchor.height = 101;
        merge.bits = 0x1e00_ffff;
        merge.version = 3;
        merge.entries_root = [4; 32];
        merge.timestamp = 1_699_999_000;
        headers.push(merge);
        // Back to the first producer: references its most recent header
        headers.push(header(14, vec![headers[3].hash()], 5));
        let bytes = encode(&headers);
        assert_eq!(decode(&bytes).unwrap(), headers);
        assert_eq!(encode(&decode(&bytes).unwrap()), bytes);
        assert_eq!(encoded_len(&headers).unwrap(), bytes.len());
        assert_eq!(decode(&encode(&[])).unwrap(), vec![]);
        // Encoding fails as well when the parents exceed the limit (symmetric with the decoding limit)
        let mut many = header(1, vec![[3; 32]; 256], 5);
        assert!(write_headers(std::slice::from_ref(&many), &mut Vec::new()).is_err());
        many.parents.pop();
        assert_eq!(
            decode(&encode(std::slice::from_ref(&many))).unwrap(),
            vec![many]
        );
    }

    /// Empty blocks do not change the state root (D-079): in a run of empty blocks only the first header writes the
    /// state root, each later one is 14 bytes; a block whose state root changes writes it as usual.
    #[test]
    fn unchanged_state_roots_are_not_repeated() {
        let mut headers = chain(100);
        for h in &mut headers {
            h.state_root = [3; 32];
        }
        // The parent hashes change with the state roots: re-link into a chain
        for i in 1..headers.len() {
            headers[i].parents = vec![headers[i - 1].hash()];
        }
        let bytes = encode(&headers);
        assert_eq!(decode(&bytes).unwrap(), headers);
        assert_eq!(encode(&decode(&bytes).unwrap()), bytes);
        assert_eq!(encoded_len(&headers).unwrap(), bytes.len());
        assert_eq!(
            bytes.len(),
            4 + (1 + 4 + 1 + 1 + 1 + 32 + 36 + 32 + 1 + 32 + 5 + 4 + 8) + 99 * 14
        );
        // A block in the middle changes the state root: it writes 32 bytes, the next block equals it and omits them
        // again
        for h in &mut headers[50..] {
            h.state_root = [4; 32];
        }
        for i in 1..headers.len() {
            headers[i].parents = vec![headers[i - 1].hash()];
        }
        let changed = encode(&headers);
        assert_eq!(decode(&changed).unwrap(), headers);
        assert_eq!(changed.len(), bytes.len() + 32);

        // Non-canonical: the first header sets the flag; equal to the previous one but the 32 bytes are written out
        let mut bad = bytes.clone();
        bad[4] |= SAME_STATE_ROOT;
        assert!(decode(&bad).is_err());
        let two = encode(&headers[..2]);
        let second = 4 + (1 + 4 + 1 + 1 + 1 + 32 + 36 + 32 + 1 + 32 + 5 + 4 + 8);
        assert_eq!(two.len(), second + 14);
        // The second header: flags, height delta, parent count and reference, (the omitted state root), producer
        // reference, timestamp delta, nonce
        let mut bad = two[..second].to_vec();
        bad.push(two[second] & !SAME_STATE_ROOT);
        bad.extend(&two[second + 1..second + 4]);
        bad.extend([3; 32]);
        bad.extend(&two[second + 4..]);
        assert!(decode(&bad).is_err());
    }

    /// Height and timestamp deltas are modulo 2^64: any value (including wrap-around and differences beyond the `i64`
    /// range) can be encoded and decoded back unchanged.
    #[test]
    fn deltas_wrap_around() {
        let mut headers = chain(4);
        for (header, (height, timestamp)) in headers.iter_mut().zip([
            (u64::MAX, 0),
            (0, u64::MAX),
            (1 << 63, 5),
            (3, (1 << 63) + 7),
        ]) {
            header.height = height;
            header.timestamp = timestamp;
        }
        let bytes = encode(&headers);
        assert_eq!(decode(&bytes).unwrap(), headers);
        assert_eq!(encode(&decode(&bytes).unwrap()), bytes);
    }

    #[test]
    fn non_canonical_encodings_are_rejected() {
        let headers = chain(2);
        let bytes = encode(&headers);
        let second = 4 + (1 + 4 + 1 + 1 + 1 + 32 + 36 + 32 + 1 + 32 + 5 + 4 + 8);
        // The first header sets a "same as previous" flag
        let mut bad = bytes.clone();
        bad[4] |= SAME_VERSION;
        assert!(decode(&bad).is_err());
        // Unknown flag bit
        let mut bad = bytes.clone();
        bad[second] |= 0x20;
        assert!(decode(&bad).is_err());
        // The second header's parent written as a full hash (it is right there in the sequence)
        let mut bad = bytes[..second].to_vec();
        let h = &headers[1];
        bad.push(SAME_VERSION | SAME_ANCHOR | SAME_BITS | EMPTY_ENTRIES);
        bad.push(2); // zigzag of height delta +1
        bad.push(1);
        bad.push(0);
        bad.extend(h.parents[0]);
        bad.extend(h.state_root);
        bad.push(1);
        bad.push(2);
        bad.extend(h.nonce.to_le_bytes());
        assert!(decode(&bad).is_err());
        // The same content decodes when the parent is referenced canonically
        let mut good = bytes[..second].to_vec();
        good.push(SAME_VERSION | SAME_ANCHOR | SAME_BITS | EMPTY_ENTRIES);
        good.extend([2, 1, 1]);
        good.extend(h.state_root);
        good.extend([1, 2]);
        good.extend(h.nonce.to_le_bytes());
        assert_eq!(good, bytes);
        // Reference out of range, truncation
        let mut bad = bytes.clone();
        bad[second + 3] = 5;
        assert!(decode(&bad).is_err());
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    }
}
