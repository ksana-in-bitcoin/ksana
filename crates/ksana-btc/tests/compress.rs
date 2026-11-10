//! The compressed form of an envelope's data (D-141): a zstd frame padded with `0x00` bytes, or the payload itself.

use bitcoin::{absolute, transaction, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, Witness};
use ksana_btc::compress::{fit, pack, unpack, SLACK, ZSTD_MAGIC};
use ksana_btc::data::{tx_data, Carrier, TxData};
use ksana_btc::envelope::envelope_script;
use ksana_primitives::DEVNET;

const MAX: usize = 80 * 1024;

fn compressible(len: usize) -> Vec<u8> {
    // a Payload starts with its variant tag (0, 1 or 2), never the zstd magic's first byte
    let mut raw = vec![2u8];
    raw.extend((0..len).map(|i| (i % 7) as u8));
    raw
}

fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut raw = vec![0u8];
    raw.extend((0..len).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state.to_le_bytes()[0]
    }));
    raw
}

#[test]
fn a_compressible_payload_goes_out_as_a_padded_frame() {
    let raw = compressible(4_000);
    let data = pack(&raw);
    assert!(data.starts_with(&ZSTD_MAGIC));
    assert!(data.len() < raw.len());
    assert!(data.ends_with(&[0u8; SLACK]));
    assert_eq!(unpack(&data, MAX).unwrap(), raw);
}

#[test]
fn an_incompressible_payload_goes_out_as_it_is() {
    let raw = noise(2_000);
    assert_eq!(pack(&raw), raw);
    assert_eq!(unpack(&raw, MAX).unwrap(), raw);
    // and the final payload of the same length too
    assert_eq!(fit(&raw, raw.len()).unwrap(), raw);
}

#[test]
fn the_final_payload_fits_the_draft_length() {
    let draft = compressible(4_000);
    let data = pack(&draft);
    // the final header differs from the drafted one (PoW nonce)
    let mut last = draft.clone();
    let tail = last.len() - 8;
    last[tail..].copy_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
    let fitted = fit(&last, data.len()).unwrap();
    assert_eq!(fitted.len(), data.len());
    assert_eq!(unpack(&fitted, MAX).unwrap(), last);
    // a frame longer than the draft's length does not fit
    assert_eq!(fit(&noise(4_000), data.len()), None);
}

#[test]
fn malformed_frames_are_rejected() {
    let raw = compressible(4_000);
    let data = pack(&raw);
    // padding other than 0x00
    let mut dirty = data.clone();
    *dirty.last_mut().unwrap() = 1;
    assert_eq!(unpack(&dirty, MAX), None);
    // a truncated frame
    let frame_len = data.len() - SLACK;
    assert_eq!(unpack(&data[..frame_len - 1], MAX), None);
    // garbage after the magic number
    assert_eq!(
        unpack(&[ZSTD_MAGIC.as_slice(), &[0xff; 16]].concat(), MAX),
        None
    );
    // a frame of nothing
    let empty = zstd::bulk::compress(&[], 3).unwrap();
    assert_eq!(unpack(&empty, MAX), None);
    // a frame needing a dictionary
    let dict = compressible(1_024);
    let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
    let with_dict = compressor.compress(&raw).unwrap();
    assert_eq!(unpack(&with_dict, MAX), None);
}

#[test]
fn a_frame_decompressing_beyond_the_bound_is_rejected() {
    let bomb = zstd::bulk::compress(&vec![0u8; 4 * MAX], 19).unwrap();
    assert!(bomb.len() < 1_024);
    assert_eq!(unpack(&bomb, MAX), None);
    assert_eq!(unpack(&bomb, 4 * MAX).unwrap().len(), 4 * MAX);
}

#[test]
fn the_tracker_reads_the_decompressed_payload_of_an_envelope() {
    let raw = compressible(4_000);
    let key =
        bitcoin::key::Keypair::from_seckey_slice(&bitcoin::key::Secp256k1::new(), &[0x5a; 32])
            .unwrap()
            .x_only_public_key()
            .0;
    let leaf = envelope_script(&key, &DEVNET.magic, &pack(&raw)).unwrap();
    let tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[vec![0u8; 64], leaf.into_bytes(), vec![0xc0; 33]]),
        }],
        output: vec![],
    };
    assert_eq!(
        tx_data(&DEVNET.magic, &tx),
        TxData::One {
            carrier: Carrier::Input(0),
            payload: raw
        }
    );
}
