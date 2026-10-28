//! Compact encoding of the entries in a payload (D-080, revising SPEC §7.1 and D-003): the bodies and this block's
//! entries in a `Settlement` or `Headers` payload are encoded in the format here, and decoding restores the full
//! entries — the `EntryData`, the VmOps in them with their Borsh encoding, signatures, `op_id` and `entries_root` are
//! all unchanged.
//!
//! | Element | Encoding |
//! | --- | --- |
//! | entry | `u8` variant (0 forced inclusion, 1 path B, 2 unlock), followed by that variant's fields |
//! | forced inclusion | the 32-byte BTC transaction ID |
//! | unlock | varint height, varint acceptance index (D-079) |
//! | path B | VmOp: `version: u8`, key `account`, varint `nonce`, fee, varint `max_gas`, action, signature; `chain_id` is not written and is taken from the network params when decoding |
//! | fee | `u8` variant: 0 `Token` followed by the varint amount; 1 `BtcNative` |
//! | action | `u8` variant: 0 `Deploy` (byte string `code`, byte string `init_args`, funds list); 1 `Call` (key `contract`, string `method`, byte string `args`, funds list); 2 `AssetTransfer` (key `asset`, key `to`, varint `amount`) |
//! | funds list | varint count, each item a key `asset` and a varint `amount` |
//! | signature | `u8` variant: 0 `Bip340` followed by 64 bytes; 1 `Bip322Simple` followed by a byte string |
//! | key | varint `r`: `r ≥ 1` is the `r`-th key in the dictionary; `r = 0` is followed by the 32 bytes, which are appended to the end of the dictionary |
//! | byte string, string | varint length and content; a string must be valid UTF-8 |
//! | entry list | varint count and the entries |
//! | body list | varint count, each body a varint header index and an entry list (D-077) |
//!
//! All integers are LEB128 varints (unsigned, minimally encoded, within the field's type). Keys are the 32-byte
//! accounts, contract IDs, asset IDs and recipients: the bodies and entries of one payload share one dictionary,
//! ordered by first occurrence, with KSA's asset ID preset (the 1st). The encoding is canonical: a key already in the
//! dictionary must be referenced, a reference cannot be out of range, varints must be minimally encoded, so for any
//! accepted input re-encoding yields the same bytes.
//!
//! A path B KSA transfer (BIP340 signature): about 143 bytes when both keys occur for the first time, about 79 bytes
//! when both are already in the dictionary (Borsh is 217 bytes).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Result, Write};

use crate::compact::{invalid, read_array, read_byte, read_varint, write_varint};
use crate::types::{Action, Body, EntryData, FeeMode, Funds, SigScheme, VmOp};
use crate::KSA_ASSET_ID;

const FORCED: u8 = 0;
const SPONSORED: u8 = 1;
const UNLOCK: u8 = 2;
const FEE_TOKEN: u8 = 0;
const FEE_NATIVE: u8 = 1;
const ACTION_DEPLOY: u8 = 0;
const ACTION_CALL: u8 = 1;
const ACTION_TRANSFER: u8 = 2;
const SIG_BIP340: u8 = 0;
const SIG_BIP322: u8 = 1;
/// The maximum number of elements preallocated when decoding (the count prefix is untrusted).
const PREALLOCATE: usize = 64;

fn write_u8<W: Write>(writer: &mut W, value: u8) -> Result<()> {
    writer.write_all(&[value])
}

/// LEB128 of a `u128`.
fn write_varint_u128<W: Write>(writer: &mut W, mut value: u128) -> Result<()> {
    loop {
        let low = u8::try_from(value & 0x7f).map_err(|_| invalid("varint encoding error"))?;
        value >>= 7;
        if value == 0 {
            return writer.write_all(&[low]);
        }
        writer.write_all(&[low | 0x80])?;
    }
}

/// LEB128 of a `u128`, required to be minimally encoded and not to exceed `u128`.
fn read_varint_u128<R: Read>(reader: &mut R) -> Result<u128> {
    let mut value: u128 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = read_byte(reader)?;
        let low = u128::from(byte & 0x7f);
        // The 19th byte (shift 126) only has room for 2 bits
        if shift == 126 && low > 3 {
            return Err(invalid("varint exceeds u128"));
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
            .filter(|s| *s <= 126)
            .ok_or_else(|| invalid("varint too long"))?;
    }
}

fn read_varint_u32<R: Read>(reader: &mut R) -> Result<u32> {
    u32::try_from(read_varint(reader)?).map_err(|_| invalid("varint exceeds u32"))
}

/// The length of a varint (bytes).
pub fn varint_len(value: u64) -> usize {
    let mut len: usize = 1;
    let mut rest = value >> 7;
    while rest > 0 {
        len = len.saturating_add(1);
        rest >>= 7;
    }
    len
}

fn write_len<W: Write>(writer: &mut W, len: usize) -> Result<()> {
    write_varint(
        writer,
        u64::try_from(len).map_err(|_| invalid("length exceeds u64"))?,
    )
}

fn write_bytes<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    write_len(writer, bytes.len())?;
    writer.write_all(bytes)
}

/// varint length and content. The length prefix is untrusted: grow with the data actually read, no preallocation.
fn read_bytes<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    let len = read_varint(reader)?;
    let mut out = Vec::new();
    Read::take(&mut *reader, len).read_to_end(&mut out)?;
    if u64::try_from(out.len()).ok() != Some(len) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "byte string truncated",
        ));
    }
    Ok(out)
}

fn read_count<R: Read>(reader: &mut R) -> Result<usize> {
    usize::try_from(read_varint(reader)?).map_err(|_| invalid("count too large"))
}

/// Entry encoder (D-080): holds the payload's key dictionary and encodes the bodies and entries in payload order.
/// [`EntryEncoder::encoded_len`] only computes the length without changing the dictionary: the producer uses it to
/// fit entries one by one within the payload limit.
#[derive(Clone, Debug)]
pub struct EntryEncoder {
    chain_id: u32,
    /// Key → reference value `r` (its index in the dictionary plus 1).
    keys: BTreeMap<[u8; 32], u64>,
}

impl EntryEncoder {
    /// A new encoder: the dictionary holds only KSA's asset ID. `chain_id` is the network's chain ID: VmOps in the
    /// payload do not write it, and a VmOp with a different chain ID cannot be encoded.
    pub fn new(chain_id: u32) -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(KSA_ASSET_ID, 1);
        EntryEncoder { chain_id, keys }
    }

    fn next_ref(&self) -> u64 {
        u64::try_from(self.keys.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1)
    }

    fn writer(&self) -> EntryWriter<'_> {
        EntryWriter {
            encoder: Some(self),
            new_keys: Vec::new(),
            out: Vec::new(),
        }
    }

    fn commit(&mut self, new_keys: Vec<[u8; 32]>) {
        for key in new_keys {
            let r = self.next_ref();
            self.keys.insert(key, r);
        }
    }

    /// The encoding of one entry; keys occurring for the first time in it are appended to the dictionary.
    pub fn encode(&mut self, entry: &EntryData) -> Result<Vec<u8>> {
        let mut writer = self.writer();
        writer.entry(entry)?;
        let EntryWriter { out, new_keys, .. } = writer;
        self.commit(new_keys);
        Ok(out)
    }

    /// The encoded length of one entry under the current dictionary (without changing the dictionary).
    pub fn encoded_len(&self, entry: &EntryData) -> Result<usize> {
        let mut writer = self.writer();
        writer.entry(entry)?;
        Ok(writer.out.len())
    }

    /// An entry list (varint count and the entries).
    pub fn encode_entries(&mut self, entries: &[EntryData]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        write_len(&mut out, entries.len())?;
        for entry in entries {
            out.extend(self.encode(entry)?);
        }
        Ok(out)
    }

    /// A body list (varint count, each a varint header index and an entry list).
    pub fn encode_bodies(&mut self, bodies: &[Body]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        write_len(&mut out, bodies.len())?;
        for body in bodies {
            write_varint(&mut out, u64::from(body.header))?;
            out.extend(self.encode_entries(&body.entries)?);
        }
        Ok(out)
    }
}

/// The maximum number of bytes an entry takes in any payload (D-080): KSA's asset ID is always a reference (1 byte),
/// every other key is written as 32 bytes as on first occurrence. Used to estimate an upper bound of the payload size
/// in advance (the budget of settlement batching and republishing).
pub fn max_entry_len(entry: &EntryData) -> Result<usize> {
    let mut writer = EntryWriter {
        encoder: None,
        new_keys: Vec::new(),
        out: Vec::new(),
    };
    writer.entry(entry)?;
    Ok(writer.out.len())
}

/// The maximum number of bytes a body takes in any payload: the varint of the header index (at the `u32` bound), the
/// varint of the entry count and the bounds of the entries.
pub fn max_body_len(entries: &[EntryData]) -> Result<usize> {
    let count = u64::try_from(entries.len()).map_err(|_| invalid("count too large"))?;
    entries.iter().try_fold(
        varint_len(u64::from(u32::MAX)).saturating_add(varint_len(count)),
        |len, entry| Ok(len.saturating_add(max_entry_len(entry)?)),
    )
}

/// The process of writing out one entry: with `encoder` `None`, computes the upper bound (see [`max_entry_len`],
/// without checking the chain ID).
struct EntryWriter<'a> {
    encoder: Option<&'a EntryEncoder>,
    /// Keys occurring for the first time in this entry (in order of occurrence, placed after the dictionary).
    new_keys: Vec<[u8; 32]>,
    out: Vec<u8>,
}

impl EntryWriter<'_> {
    fn key(&mut self, key: &[u8; 32]) -> Result<()> {
        let Some(encoder) = self.encoder else {
            if *key == KSA_ASSET_ID {
                return write_varint(&mut self.out, 1);
            }
            write_varint(&mut self.out, 0)?;
            return self.out.write_all(key);
        };
        if let Some(r) = encoder.keys.get(key) {
            return write_varint(&mut self.out, *r);
        }
        if let Some(position) = self.new_keys.iter().position(|k| k == key) {
            let r = encoder
                .next_ref()
                .saturating_add(u64::try_from(position).unwrap_or(u64::MAX));
            return write_varint(&mut self.out, r);
        }
        write_varint(&mut self.out, 0)?;
        self.out.write_all(key)?;
        self.new_keys.push(*key);
        Ok(())
    }

    fn entry(&mut self, entry: &EntryData) -> Result<()> {
        match entry {
            EntryData::Forced { btc_txid } => {
                write_u8(&mut self.out, FORCED)?;
                self.out.write_all(btc_txid)
            }
            EntryData::Sponsored { op } => {
                write_u8(&mut self.out, SPONSORED)?;
                self.op(op)
            }
            EntryData::Unlock { height, index } => {
                write_u8(&mut self.out, UNLOCK)?;
                write_varint(&mut self.out, *height)?;
                write_varint(&mut self.out, u64::from(*index))
            }
        }
    }

    fn op(&mut self, op: &VmOp) -> Result<()> {
        if self.encoder.is_some_and(|e| op.chain_id != e.chain_id) {
            return Err(invalid(
                "VmOp chain_id does not match the network and cannot be written into a payload",
            ));
        }
        write_u8(&mut self.out, op.version)?;
        self.key(&op.account)?;
        write_varint(&mut self.out, op.nonce)?;
        match op.fee {
            FeeMode::Token { amount } => {
                write_u8(&mut self.out, FEE_TOKEN)?;
                write_varint_u128(&mut self.out, amount)?;
            }
            FeeMode::BtcNative => write_u8(&mut self.out, FEE_NATIVE)?,
        }
        write_varint(&mut self.out, op.max_gas)?;
        self.action(&op.action)?;
        match &op.sig {
            SigScheme::Bip340(sig) => {
                write_u8(&mut self.out, SIG_BIP340)?;
                self.out.write_all(sig)
            }
            SigScheme::Bip322Simple(sig) => {
                write_u8(&mut self.out, SIG_BIP322)?;
                write_bytes(&mut self.out, sig)
            }
        }
    }

    fn action(&mut self, action: &Action) -> Result<()> {
        match action {
            Action::Deploy {
                code,
                init_args,
                funds,
            } => {
                write_u8(&mut self.out, ACTION_DEPLOY)?;
                write_bytes(&mut self.out, code)?;
                write_bytes(&mut self.out, init_args)?;
                self.funds(funds)
            }
            Action::Call {
                contract,
                method,
                args,
                funds,
            } => {
                write_u8(&mut self.out, ACTION_CALL)?;
                self.key(contract)?;
                write_bytes(&mut self.out, method.as_bytes())?;
                write_bytes(&mut self.out, args)?;
                self.funds(funds)
            }
            Action::AssetTransfer { asset, to, amount } => {
                write_u8(&mut self.out, ACTION_TRANSFER)?;
                self.key(asset)?;
                self.key(to)?;
                write_varint_u128(&mut self.out, *amount)
            }
        }
    }

    fn funds(&mut self, funds: &[Funds]) -> Result<()> {
        write_len(&mut self.out, funds.len())?;
        for f in funds {
            self.key(&f.asset)?;
            write_varint_u128(&mut self.out, f.amount)?;
        }
        Ok(())
    }
}

/// Entry decoder (D-080): the counterpart of [`EntryEncoder`], reads the bodies and entries in the same order and
/// checks that the encoding is canonical.
#[derive(Clone, Debug)]
pub struct EntryDecoder {
    chain_id: u32,
    keys: Vec<[u8; 32]>,
    known: BTreeSet<[u8; 32]>,
}

impl EntryDecoder {
    /// A new decoder: the dictionary holds only KSA's asset ID; the restored VmOps get chain ID `chain_id` (network
    /// params).
    pub fn new(chain_id: u32) -> Self {
        EntryDecoder {
            chain_id,
            keys: vec![KSA_ASSET_ID],
            known: BTreeSet::from([KSA_ASSET_ID]),
        }
    }

    fn key<R: Read>(&mut self, reader: &mut R) -> Result<[u8; 32]> {
        let r = read_varint(reader)?;
        if r == 0 {
            let key = read_array(reader)?;
            if !self.known.insert(key) {
                return Err(invalid(
                    "a key already in the dictionary must be referenced (non-canonical encoding)",
                ));
            }
            self.keys.push(key);
            return Ok(key);
        }
        let position = usize::try_from(r.saturating_sub(1))
            .map_err(|_| invalid("key reference out of range"))?;
        self.keys
            .get(position)
            .copied()
            .ok_or_else(|| invalid("key reference out of range"))
    }

    /// Read one entry.
    pub fn entry<R: Read>(&mut self, reader: &mut R) -> Result<EntryData> {
        match read_byte(reader)? {
            FORCED => Ok(EntryData::Forced {
                btc_txid: read_array(reader)?,
            }),
            SPONSORED => Ok(EntryData::Sponsored {
                op: self.op(reader)?,
            }),
            UNLOCK => Ok(EntryData::Unlock {
                height: read_varint(reader)?,
                index: read_varint_u32(reader)?,
            }),
            _ => Err(invalid("unknown entry variant")),
        }
    }

    fn op<R: Read>(&mut self, reader: &mut R) -> Result<VmOp> {
        let version = read_byte(reader)?;
        let account = self.key(reader)?;
        let nonce = read_varint(reader)?;
        let fee = match read_byte(reader)? {
            FEE_TOKEN => FeeMode::Token {
                amount: read_varint_u128(reader)?,
            },
            FEE_NATIVE => FeeMode::BtcNative,
            _ => return Err(invalid("unknown fee mode")),
        };
        let max_gas = read_varint(reader)?;
        let action = self.action(reader)?;
        let sig = match read_byte(reader)? {
            SIG_BIP340 => {
                let mut sig = [0u8; 64];
                reader.read_exact(&mut sig)?;
                SigScheme::Bip340(sig)
            }
            SIG_BIP322 => SigScheme::Bip322Simple(read_bytes(reader)?),
            _ => return Err(invalid("unknown signature scheme")),
        };
        Ok(VmOp {
            version,
            chain_id: self.chain_id,
            account,
            nonce,
            fee,
            max_gas,
            action,
            sig,
        })
    }

    fn action<R: Read>(&mut self, reader: &mut R) -> Result<Action> {
        match read_byte(reader)? {
            ACTION_DEPLOY => Ok(Action::Deploy {
                code: read_bytes(reader)?,
                init_args: read_bytes(reader)?,
                funds: self.funds(reader)?,
            }),
            ACTION_CALL => {
                let contract = self.key(reader)?;
                let method = String::from_utf8(read_bytes(reader)?)
                    .map_err(|_| invalid("method name is not valid UTF-8"))?;
                Ok(Action::Call {
                    contract,
                    method,
                    args: read_bytes(reader)?,
                    funds: self.funds(reader)?,
                })
            }
            ACTION_TRANSFER => Ok(Action::AssetTransfer {
                asset: self.key(reader)?,
                to: self.key(reader)?,
                amount: read_varint_u128(reader)?,
            }),
            _ => Err(invalid("unknown action")),
        }
    }

    fn funds<R: Read>(&mut self, reader: &mut R) -> Result<Vec<Funds>> {
        let count = read_count(reader)?;
        let mut funds = Vec::with_capacity(count.min(PREALLOCATE));
        for _ in 0..count {
            funds.push(Funds {
                asset: self.key(reader)?,
                amount: read_varint_u128(reader)?,
            });
        }
        Ok(funds)
    }

    /// Read an entry list.
    pub fn entries<R: Read>(&mut self, reader: &mut R) -> Result<Vec<EntryData>> {
        let count = read_count(reader)?;
        let mut entries = Vec::with_capacity(count.min(PREALLOCATE));
        for _ in 0..count {
            entries.push(self.entry(reader)?);
        }
        Ok(entries)
    }

    /// Read a body list (the caller checks canonicality separately with [`crate::check_bodies`]).
    pub fn bodies<R: Read>(&mut self, reader: &mut R) -> Result<Vec<Body>> {
        let count = read_count(reader)?;
        let mut bodies = Vec::with_capacity(count.min(PREALLOCATE));
        for _ in 0..count {
            let header = read_varint_u32(reader)?;
            bodies.push(Body {
                header,
                entries: self.entries(reader)?,
            });
        }
        Ok(bodies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Params, Payload, DEVNET, VMOP_VERSION};

    fn transfer(account: u8, to: u8, nonce: u64) -> EntryData {
        EntryData::Sponsored {
            op: VmOp {
                version: VMOP_VERSION,
                chain_id: DEVNET.chain_id,
                account: [account; 32],
                nonce,
                fee: FeeMode::Token { amount: 2 },
                max_gas: 100_000,
                action: Action::AssetTransfer {
                    asset: KSA_ASSET_ID,
                    to: [to; 32],
                    amount: 1_000,
                },
                sig: SigScheme::Bip340([7; 64]),
            },
        }
    }

    fn decode_all(chain_id: u32, bytes: &[u8]) -> Result<Vec<EntryData>> {
        let mut reader = bytes;
        let entries = EntryDecoder::new(chain_id).entries(&mut reader)?;
        if !reader.is_empty() {
            return Err(invalid("trailing bytes"));
        }
        Ok(entries)
    }

    /// A path B KSA transfer: Borsh 217 bytes; 143 bytes when both keys occur for the first time, 79 bytes when both
    /// are already in the dictionary (D-080's estimate was 145 / 85).
    #[test]
    fn a_transfer_shrinks_with_the_key_dictionary() {
        let first = transfer(1, 2, 0);
        assert_eq!(borsh::to_vec(&first).unwrap().len(), 217);
        let mut encoder = EntryEncoder::new(DEVNET.chain_id);
        // variant 1 + version 1 + account 33 + nonce 1 + fee variant 1 and amount 1 + max_gas 3 + action 1 + KSA
        // reference 1 + recipient 33 + amount 2 + signature 65
        assert_eq!(encoder.encoded_len(&first).unwrap(), 143);
        assert_eq!(max_entry_len(&first).unwrap(), 143);
        assert_eq!(encoder.encode(&first).unwrap().len(), 143);
        // The next one between the same two accounts: both reference the dictionary
        let again = transfer(1, 2, 1);
        assert_eq!(encoder.encoded_len(&again).unwrap(), 79);
        assert_eq!(
            max_entry_len(&again).unwrap(),
            143,
            "the upper bound does not depend on the dictionary"
        );
        // The recipient transfers back: they reference each other
        let back = transfer(2, 1, 0);
        let entries = vec![first.clone(), again.clone(), back.clone()];
        let bytes = EntryEncoder::new(DEVNET.chain_id)
            .encode_entries(&entries)
            .unwrap();
        assert_eq!(bytes.len(), 1 + 143 + 79 + 79);
        assert_eq!(decode_all(DEVNET.chain_id, &bytes).unwrap(), entries);
        // The decoder fills the chain ID back in: decoding on another network yields another chain ID
        let other = decode_all(1002, &bytes).unwrap();
        assert!(matches!(&other[0], EntryData::Sponsored { op } if op.chain_id == 1002));
    }

    /// A key occurring a second time within the same entry references the position just added to the dictionary by
    /// this entry; the length computation matches the actual encoding and does not change the dictionary.
    #[test]
    fn keys_new_in_an_entry_are_referenced_within_it() {
        let self_transfer = transfer(5, 5, 0);
        let encoder = EntryEncoder::new(DEVNET.chain_id);
        let len = encoder.encoded_len(&self_transfer).unwrap();
        assert_eq!(len, 143 - 32);
        assert_eq!(encoder.encoded_len(&self_transfer).unwrap(), len);
        let mut encoder = encoder;
        assert_eq!(encoder.encode(&self_transfer).unwrap().len(), len);
        assert_eq!(encoder.encoded_len(&self_transfer).unwrap(), 79);
    }

    #[test]
    fn every_variant_roundtrips() {
        let call = EntryData::Sponsored {
            op: VmOp {
                version: VMOP_VERSION,
                chain_id: DEVNET.chain_id,
                account: [9; 32],
                nonce: u64::MAX,
                fee: FeeMode::BtcNative,
                max_gas: 0,
                action: Action::Call {
                    contract: [8; 32],
                    method: "échanger".into(),
                    args: vec![1, 2, 3],
                    funds: vec![
                        Funds {
                            asset: KSA_ASSET_ID,
                            amount: u128::MAX,
                        },
                        Funds {
                            asset: [9; 32],
                            amount: 1,
                        },
                    ],
                },
                sig: SigScheme::Bip322Simple(vec![4; 70]),
            },
        };
        let deploy = EntryData::Sponsored {
            op: VmOp {
                version: 1,
                chain_id: DEVNET.chain_id,
                account: [8; 32],
                nonce: 0,
                fee: FeeMode::Token { amount: 0 },
                max_gas: u64::MAX,
                action: Action::Deploy {
                    code: vec![0; 300],
                    init_args: vec![],
                    funds: vec![],
                },
                sig: SigScheme::Bip340([1; 64]),
            },
        };
        let entries = vec![
            EntryData::Forced { btc_txid: [3; 32] },
            EntryData::Unlock {
                height: u64::MAX,
                index: u32::MAX,
            },
            call,
            deploy,
            transfer(9, 8, 3),
        ];
        let bytes = EntryEncoder::new(DEVNET.chain_id)
            .encode_entries(&entries)
            .unwrap();
        assert_eq!(decode_all(DEVNET.chain_id, &bytes).unwrap(), entries);
        for entry in &entries {
            let alone = EntryEncoder::new(DEVNET.chain_id).encode(entry).unwrap();
            assert!(alone.len() <= max_entry_len(entry).unwrap());
        }
    }

    /// Every non-canonical encoding is rejected: a key already in the dictionary written in full, reference out of
    /// range, non-minimal varint, integers beyond the type's range, unknown variants, non-UTF-8 method name.
    #[test]
    fn non_canonical_entries_are_rejected() {
        let entries = vec![transfer(1, 2, 0), transfer(1, 2, 1)];
        let bytes = EntryEncoder::new(DEVNET.chain_id)
            .encode_entries(&entries)
            .unwrap();
        // count 1; the first one: variant 1, version 1, account r=0 and 32 bytes
        let account_at = 3;
        assert_eq!(bytes[account_at], 0);
        let second = 1 + 143;
        let second_account = second + 2;
        assert_eq!(
            bytes[second_account], 2,
            "the account is the 2nd key in the dictionary"
        );
        let check = |bad: Vec<u8>, message: &str| match decode_all(DEVNET.chain_id, &bad) {
            Err(e) => assert!(
                e.to_string().contains(message),
                "{e} must contain {message}"
            ),
            Ok(v) => panic!("must be rejected ({message}): {v:?}"),
        };
        // The second one writes the account in the dictionary in full
        let mut full = bytes[..second_account].to_vec();
        full.push(0);
        full.extend([1; 32]);
        full.extend(&bytes[second_account + 1..]);
        check(full, "a key already in the dictionary must be referenced");
        // Reference out of range (the dictionary only has KSA and the two accounts at this point)
        let mut far = bytes.clone();
        far[second_account] = 4;
        check(far, "key reference out of range");
        // KSA cannot be written in full either
        let mut ksa_full = bytes.clone();
        let asset_at = second + 1 + 1 + 1 + 1 + 1 + 1 + 3 + 1;
        assert_eq!(ksa_full[asset_at], 1);
        ksa_full.splice(asset_at..=asset_at, [0; 33]);
        check(
            ksa_full,
            "a key already in the dictionary must be referenced",
        );
        // Non-minimal varint (nonce 1 written as 0x81 0x00)
        let mut long = bytes.clone();
        let nonce_at = second_account + 1;
        long.splice(nonce_at..=nonce_at, [0x81, 0x00]);
        check(long, "varint is not minimally encoded");
        // Unknown entry variant and action
        check(vec![1, 3], "unknown entry variant");
        let mut action = bytes.clone();
        action[second + 1 + 1 + 1 + 1 + 1 + 1 + 3] = 3;
        check(action, "unknown action");
        // Acceptance index exceeds u32, amount exceeds u128
        let mut index = vec![1, 2, 0];
        index.extend([0xff, 0xff, 0xff, 0xff, 0x10]);
        check(index, "varint exceeds u32");
        let mut amount = vec![0xff; 18];
        amount.push(0x04);
        assert!(read_varint_u128(&mut amount.as_slice()).is_err());
        let mut max = vec![0xff; 18];
        max.push(0x03);
        assert_eq!(read_varint_u128(&mut max.as_slice()).unwrap(), u128::MAX);
        // Method name is not UTF-8
        let call = EntryData::Sponsored {
            op: VmOp {
                version: VMOP_VERSION,
                chain_id: DEVNET.chain_id,
                account: [1; 32],
                nonce: 0,
                fee: FeeMode::BtcNative,
                max_gas: 1,
                action: Action::Call {
                    contract: [2; 32],
                    method: "ab".into(),
                    args: vec![],
                    funds: vec![],
                },
                sig: SigScheme::Bip340([0; 64]),
            },
        };
        let mut bytes = EntryEncoder::new(DEVNET.chain_id)
            .encode_entries(&[call])
            .unwrap();
        let method_at = bytes.windows(2).position(|w| w == b"ab").unwrap();
        bytes[method_at] = 0xff;
        check(bytes, "method name is not valid UTF-8");
        // Truncated byte string
        check(vec![1, 1, 0], "");
    }

    /// A VmOp whose chain ID differs from the network's cannot be written into a payload (it would be restored with
    /// the network's chain ID when decoding).
    #[test]
    fn ops_of_another_chain_cannot_be_encoded() {
        let mut entry = transfer(1, 2, 0);
        if let EntryData::Sponsored { op } = &mut entry {
            op.chain_id = 7;
        }
        assert!(EntryEncoder::new(DEVNET.chain_id).encode(&entry).is_err());
        let payload = Payload::Settlement {
            headers: Vec::new(),
            entries: vec![entry],
            bodies: Vec::new(),
        };
        assert!(payload.encode(&DEVNET).is_err());
        let other = Params {
            chain_id: 7,
            ..DEVNET
        };
        let bytes = payload.encode(&other).unwrap();
        assert_eq!(Payload::decode(&bytes, &other).unwrap(), payload);
    }

    #[test]
    fn varint_lengths() {
        for (value, len) in [
            (0, 1),
            (127, 1),
            (128, 2),
            (16_383, 2),
            (16_384, 3),
            (u64::MAX, 10),
        ] {
            assert_eq!(varint_len(value), len);
            let mut out = Vec::new();
            write_varint(&mut out, value).unwrap();
            assert_eq!(out.len(), len);
        }
    }
}
