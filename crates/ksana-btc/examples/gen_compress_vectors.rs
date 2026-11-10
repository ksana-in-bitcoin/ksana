//! Generates `tests/vectors/compress.json`, the envelope data vectors shared by Rust and the TypeScript SDK (D-141):
//! for each case the envelope's data, the bound on the payload's length and the payload [`ksana_btc::compress::unpack`]
//! reads from it (`null` when the envelope is malformed).
//!
//! `cargo run -p ksana-btc --example gen_compress_vectors`
//!
//! Once committed, the vectors are the reference: `crates/ksana-btc/tests/compress.rs` and the SDK's
//! `test/compress.test.ts` check them case by case.

use ksana_btc::compress::{unpack, ZSTD_MAGIC};
use serde_json::{json, Value};

/// A payload-like byte string: a variant tag, then bytes that repeat.
fn sample(len: usize) -> Vec<u8> {
    let mut raw = vec![2u8];
    raw.extend((0..len).map(|i| (i % 7) as u8 ^ (i / 97) as u8));
    raw
}

/// A zstd frame from its header fields (after the magic number), its blocks and its checksum.
fn frame(header: &[u8], blocks: &[Vec<u8>], checksum: &[u8]) -> Vec<u8> {
    [ZSTD_MAGIC.as_slice(), header, &blocks.concat(), checksum].concat()
}

/// A block: its 3-byte header (`last`, type 0 raw / 1 RLE / 2 compressed / 3 reserved, 21-bit size) and its content.
fn block(last: bool, kind: u32, size: u32, content: &[u8]) -> Vec<u8> {
    let head = (size << 3) | (kind << 1) | u32::from(last);
    [&head.to_le_bytes()[..3], content].concat()
}

fn raw_block(last: bool, content: &[u8]) -> Vec<u8> {
    block(last, 0, content.len() as u32, content)
}

struct Case {
    name: &'static str,
    note: &'static str,
    max: usize,
    data: Vec<u8>,
}

fn main() {
    let text = sample(600);
    let level19 = zstd::bulk::compress(&text, 19).expect("compress");
    let mut checked = zstd::bulk::Compressor::new(3).expect("compressor");
    checked.include_checksum(true).expect("checksum flag");
    let with_checksum = checked.compress(&text).expect("compress");
    let mut wrong_checksum = with_checksum.clone();
    *wrong_checksum.last_mut().expect("a frame") ^= 1;
    // a raw-content dictionary: the frame names no dictionary, yet its matches reach before its first byte
    let mut referring = zstd::bulk::Compressor::with_dictionary(19, &text).expect("compressor");
    let needs_content = referring.compress(&text).expect("compress");
    let abc = [2u8, 0xaa, 0xbb, 0xcc];
    // single segment, a 1-byte content size
    let single = |size: u8, blocks: &[Vec<u8>]| frame(&[0x20, size], blocks, &[]);
    // a window descriptor and no content size
    let windowed = |descriptor: u8, blocks: &[Vec<u8>]| frame(&[0x00, descriptor], blocks, &[]);

    let cases = vec![
        Case {
            name: "payload-itself",
            note: "data that does not start with the zstd magic number is the payload",
            max: 64,
            data: abc.to_vec(),
        },
        Case {
            name: "skippable-frame-magic",
            note: "a skippable frame's magic number is not the zstd magic number: the data is the payload",
            max: 64,
            data: vec![0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0],
        },
        Case {
            name: "frame",
            note: "one frame as the node writes it (level 19)",
            max: 1024,
            data: level19.clone(),
        },
        Case {
            name: "frame-padded",
            note: "the frame followed by 32 bytes of 0x00",
            max: 1024,
            data: [level19.as_slice(), &[0; 32]].concat(),
        },
        Case {
            name: "frame-padding-not-zero",
            note: "padding other than 0x00",
            max: 1024,
            data: [level19.as_slice(), &[0; 31], &[1]].concat(),
        },
        Case {
            name: "frame-twice",
            note: "a second frame after the first is padding other than 0x00",
            max: 4096,
            data: [level19.as_slice(), level19.as_slice()].concat(),
        },
        Case {
            name: "frame-truncated",
            note: "the frame without its last byte",
            max: 1024,
            data: level19[..level19.len() - 1].to_vec(),
        },
        Case {
            name: "frame-at-the-bound",
            note: "a payload of exactly the bound",
            max: text.len(),
            data: level19.clone(),
        },
        Case {
            name: "frame-beyond-the-bound",
            note: "a payload one byte longer than the bound",
            max: text.len() - 1,
            data: level19,
        },
        Case {
            name: "checksum",
            note: "a frame with a content checksum",
            max: 1024,
            data: with_checksum,
        },
        Case {
            name: "checksum-wrong",
            note: "a content checksum that does not match",
            max: 1024,
            data: wrong_checksum,
        },
        Case {
            name: "matches-before-the-start",
            note: "a frame compressed against a raw-content dictionary: it names no dictionary and its matches reach before its first byte",
            max: 1024,
            data: needs_content,
        },
        Case {
            name: "raw-block",
            note: "single segment, content size 4, one raw block",
            max: 64,
            data: single(4, &[raw_block(true, &abc)]),
        },
        Case {
            name: "rle-block",
            note: "single segment, content size 9, a raw block and an RLE block",
            max: 64,
            data: single(9, &[raw_block(false, &abc), block(true, 1, 5, &[0x11])]),
        },
        Case {
            name: "window-no-content-size",
            note: "a 1 KiB window, no content size, an empty block between two raw blocks",
            max: 64,
            data: windowed(0x00, &[raw_block(false, &abc), raw_block(false, &[]), raw_block(true, &abc)]),
        },
        Case {
            name: "content-size-2-bytes",
            note: "a 2-byte content size (the value less 256)",
            max: 1024,
            data: frame(&[0x40, 0x00, 0x2c, 0x00], &[block(true, 1, 300, &[2])], &[]),
        },
        Case {
            name: "content-size-4-bytes",
            note: "a 4-byte content size",
            max: 64,
            data: frame(&[0x80, 0x00, 4, 0, 0, 0], &[raw_block(true, &abc)], &[]),
        },
        Case {
            name: "content-size-8-bytes",
            note: "an 8-byte content size",
            max: 64,
            data: frame(&[0xc0, 0x00, 4, 0, 0, 0, 0, 0, 0, 0], &[raw_block(true, &abc)], &[]),
        },
        Case {
            name: "content-size-8-bytes-huge",
            note: "an 8-byte content size of 2^32 + 4 over 4 bytes of content",
            max: 64,
            data: frame(&[0xc0, 0x00, 4, 0, 0, 0, 1, 0, 0, 0], &[raw_block(true, &abc)], &[]),
        },
        Case {
            name: "content-size-above",
            note: "a content size of 5 over 4 bytes of content",
            max: 64,
            data: single(5, &[raw_block(true, &abc)]),
        },
        Case {
            name: "content-size-below",
            note: "a content size of 3 over 4 bytes of content",
            max: 64,
            data: single(3, &[raw_block(true, &abc)]),
        },
        Case {
            name: "nothing",
            note: "a frame of no content",
            max: 64,
            data: single(0, &[raw_block(true, &[])]),
        },
        Case {
            name: "nothing-no-content-size",
            note: "a frame of no content that states no content size",
            max: 64,
            data: windowed(0x00, &[raw_block(true, &[])]),
        },
        Case {
            name: "rle-at-the-bound",
            note: "an RLE block of exactly the bound",
            max: 1000,
            data: windowed(0x00, &[block(true, 1, 1000, &[2])]),
        },
        Case {
            name: "rle-beyond-the-bound",
            note: "an RLE block one byte longer than the bound",
            max: 999,
            data: windowed(0x00, &[block(true, 1, 1000, &[2])]),
        },
        Case {
            name: "blocks-beyond-the-bound",
            note: "RLE blocks of 2 MiB less one byte each, 4 bytes apiece",
            max: 400_000,
            data: windowed(
                0x50,
                &[
                    block(false, 1, 0x1f_ffff, &[2]),
                    block(false, 1, 0x1f_ffff, &[2]),
                    block(true, 1, 0x1f_ffff, &[2]),
                ],
            ),
        },
        Case {
            name: "rle-block-longer-than-the-window",
            note: "a 1 KiB window and an RLE block of 2 000 bytes",
            max: 4096,
            data: windowed(0x00, &[block(true, 1, 2000, &[2])]),
        },
        Case {
            name: "raw-block-longer-than-the-window",
            note: "a 1 KiB window and a raw block of 2 000 bytes",
            max: 4096,
            data: windowed(0x00, &[raw_block(true, &sample(1999))]),
        },
        Case {
            name: "window-largest",
            note: "the largest window the node's decoder takes (window log 31, mantissa 7)",
            max: 64,
            data: windowed(0xaf, &[raw_block(true, &abc)]),
        },
        Case {
            name: "window-too-large",
            note: "a window log of 32",
            max: 64,
            data: windowed(0xb0, &[raw_block(true, &abc)]),
        },
        Case {
            name: "reserved-bit",
            note: "the reserved bit of the frame header descriptor",
            max: 64,
            data: frame(&[0x28, 4], &[raw_block(true, &abc)], &[]),
        },
        Case {
            name: "unused-bit",
            note: "the unused bit of the frame header descriptor",
            max: 64,
            data: frame(&[0x30, 4], &[raw_block(true, &abc)], &[]),
        },
        Case {
            name: "dictionary-id-zero",
            note: "a dictionary ID field of 0: no dictionary",
            max: 64,
            data: frame(&[0x21, 0, 4], &[raw_block(true, &abc)], &[]),
        },
        Case {
            name: "dictionary-id",
            note: "a frame that names a dictionary",
            max: 64,
            data: frame(&[0x21, 7, 4], &[raw_block(true, &abc)], &[]),
        },
        Case {
            name: "dictionary-id-4-bytes",
            note: "a 4-byte dictionary ID",
            max: 64,
            data: frame(&[0x23, 0, 0, 0, 1, 4], &[raw_block(true, &abc)], &[]),
        },
        Case {
            name: "reserved-block-type",
            note: "a block of the reserved type",
            max: 64,
            data: single(4, &[block(true, 3, 4, &abc)]),
        },
        Case {
            name: "no-last-block",
            note: "the data ends before a last block",
            max: 64,
            data: single(4, &[raw_block(false, &abc)]),
        },
        Case {
            name: "header-truncated",
            note: "the magic number and half a header",
            max: 64,
            data: frame(&[0xc0, 0x00, 4, 0], &[], &[]),
        },
        Case {
            name: "magic-only",
            note: "the magic number alone",
            max: 64,
            data: ZSTD_MAGIC.to_vec(),
        },
    ];

    let cases: Vec<Value> = cases
        .iter()
        .map(|case| {
            json!({
                "name": case.name,
                "note": case.note,
                "max": case.max,
                "data": hex::encode(&case.data),
                "payload": unpack(&case.data, case.max).map(hex::encode),
            })
        })
        .collect();
    let vectors = json!({
        "description": "Ksana envelope data vectors (D-141): data is the data of a KSN1 envelope (its chunks concatenated), max the bound on the payload's length, payload the payload read from it: the data itself, or the content of its zstd frame; null when the envelope is malformed",
        "cases": cases,
    });
    let path = format!(
        "{}/../../tests/vectors/compress.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut text = serde_json::to_string_pretty(&vectors).expect("JSON");
    text.push('\n');
    std::fs::write(&path, text).expect("write vectors");
    println!("wrote {path}: {} cases", cases.len());
}
