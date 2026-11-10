//! Compression of the payload in a KSN1 envelope (D-141): the envelope's data is either the payload itself or one
//! zstd frame (RFC 8878, no dictionary) of it followed by zero or more `0x00` bytes of padding. A payload is a Borsh
//! [`ksana_primitives::Payload`] whose first byte (the variant tag) is never the first byte of the zstd magic number,
//! so the two forms cannot be confused. Validation compares the decompressed payload with the canonical one, never the
//! compressed bytes, so the result does not depend on the compressor's version or level.
//!
//! The producer drafts its settlement before solving the PoW, and the final header changes the compressed length: the
//! draft reserves [`SLACK`] bytes beyond the draft's compressed length and [`fit`] pads the final frame to it.

/// The zstd frame magic number (RFC 8878 §3.1.1, little-endian `0xFD2FB528`).
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// The compression level the node uses (any level decompresses the same).
pub const LEVEL: i32 = 19;

/// The bytes a drafted compressed payload reserves beyond its compressed length for the final header.
pub const SLACK: usize = 32;

/// The envelope data for `raw` when drafting: one zstd frame padded by [`SLACK`] bytes when that is shorter than
/// `raw`, otherwise `raw` itself.
pub fn pack(raw: &[u8]) -> Vec<u8> {
    match zstd::bulk::compress(raw, LEVEL) {
        Ok(frame) if frame.len().saturating_add(SLACK) < raw.len() => {
            let len = frame.len().saturating_add(SLACK);
            padded(frame, len)
        }
        _ => raw.to_vec(),
    }
}

/// The envelope data for the final payload `raw` of exactly `len` bytes, the length [`pack`] gave the draft's: `raw`
/// itself when it has that length, otherwise its zstd frame padded to it; `None` when the frame is longer.
pub fn fit(raw: &[u8], len: usize) -> Option<Vec<u8>> {
    if raw.len() == len {
        return Some(raw.to_vec());
    }
    let frame = zstd::bulk::compress(raw, LEVEL).ok()?;
    (frame.len() <= len).then(|| padded(frame, len))
}

/// The payload in envelope data `data`: `data` itself when it does not start with [`ZSTD_MAGIC`], otherwise the
/// decompressed frame. `None` when the frame is malformed, needs a dictionary, decompresses to more than `max` bytes
/// or to nothing, or is followed by anything other than `0x00` bytes.
pub fn unpack(data: &[u8], max: usize) -> Option<Vec<u8>> {
    if !data.starts_with(&ZSTD_MAGIC) {
        return Some(data.to_vec());
    }
    let len = zstd::zstd_safe::find_frame_compressed_size(data).ok()?;
    let (frame, padding) = (data.get(..len)?, data.get(len..)?);
    if padding.iter().any(|&b| b != 0) {
        return None;
    }
    let raw = zstd::bulk::decompress(frame, max).ok()?;
    (!raw.is_empty() && raw.len() <= max).then_some(raw)
}

fn padded(mut frame: Vec<u8>, len: usize) -> Vec<u8> {
    frame.resize(len, 0);
    frame
}
