//! The correspondence between blocks and settlement payloads (SPEC §6.3, §7.1; D-028, D-034, D-043, D-077, D-081):
//! which blocks' bodies must land on BTC, the encoding and length of settlement payloads, the bodies carried by a
//! payload, the payload in a settlement transaction (the reveal transaction's envelope) and the funding inputs. Shared
//! by node and indexer.

use bitcoin::hashes::Hash;
use ksana_primitives::compact_entries;
use ksana_primitives::{compact, BlockHeader, Body, Entry, EntryData, Params, Payload};

use crate::data::{Carrier, TxData};
use crate::settlement::SettlementPair;

/// Payload errors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PayloadError {
    /// The payload exceeds `MAX_BLOCK_DATA_BYTES` (invalid block).
    #[error("settlement payload of {0} bytes exceeds MAX_BLOCK_DATA_BYTES")]
    TooLarge(usize),
    /// Encoding failed (internal error).
    #[error("settlement payload encoding failed: {0}")]
    Encode(String),
}

/// Whether a block's body must land on BTC (§6.3, §7, D-043, D-077): it contains any entry other than forced
/// inclusions (path B, unlock).
/// Empty blocks and blocks containing only forced-inclusion entries only need their header on BTC: their entries can
/// be derived from BTC (§7.4).
pub fn needs_body(entries: &[Entry]) -> bool {
    entries.iter().any(|e| !matches!(e, Entry::Forced { .. }))
}

/// Whether a block contains fast-path entries (path B).
pub fn has_fast_path(entries: &[Entry]) -> bool {
    entries.iter().any(|e| matches!(e, Entry::Sponsored { .. }))
}

/// The settlement payload of a block (headers and entries compressed, D-075, D-079, D-080). The last of `headers` is
/// this block's header, the rest are headers not yet landed that the producer chose to carry (D-063), limited by the
/// caller to at most `MAX_SETTLEMENT_HEADERS`; `bodies` are the bodies of some of those blocks (D-077, must satisfy
/// [`ksana_primitives::check_bodies`], otherwise the encoded payload cannot be decoded).
pub fn payload(
    params: &Params,
    headers: Vec<BlockHeader>,
    entries: &[Entry],
    bodies: Vec<Body>,
) -> Result<Vec<u8>, PayloadError> {
    ksana_primitives::check_bodies(&headers, &bodies, true).map_err(PayloadError::Encode)?;
    let bytes = Payload::Settlement {
        headers,
        entries: entries.iter().map(EntryData::from).collect(),
        bodies,
    }
    .encode(params)
    .map_err(|e| PayloadError::Encode(e.to_string()))?;
    if bytes.len() > params.max_block_data_bytes {
        return Err(PayloadError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Whether a settlement payload belongs to block `header` (D-063): its length does not exceed
/// `MAX_BLOCK_DATA_BYTES`, it decodes as `Settlement`, it has 1 to `MAX_SETTLEMENT_HEADERS` headers with the last one
/// being that block's header, every header anchors at or below that block's anchor height, and its entries equal
/// `entries` one by one. The other headers may be any such headers (other headers not yet landed that the producer
/// chose to carry, which are in the block's past, whose anchors never exceed its own; headers prove themselves by PoW,
/// as in republish payloads). The anchor bound (D-100) makes the payload count ([`crate::tracker::payload_counts`])
/// wherever it confirms on a chain holding the block's anchor, since it commits to that anchor and confirms above it;
/// a header anchored higher would keep the payload from ever counting, and the block's data from landing, while the
/// block stays valid (second audit P4b-1). Returns the other bodies carried by the payload (D-077: (header, entries),
/// checked by the node against the blocks in this block's past).
pub fn settlement_belongs_to(
    params: &Params,
    payload: &[u8],
    header: &BlockHeader,
    entries: &[EntryData],
) -> Result<Vec<(BlockHeader, Vec<EntryData>)>, String> {
    let decoded = Payload::decode(payload, params)
        .map_err(|e| format!("settlement payload cannot be decoded: {e}"))?;
    let Payload::Settlement {
        headers,
        entries: carried,
        bodies,
    } = decoded
    else {
        return Err("payload is not a settlement payload".into());
    };
    if headers.len() > params.max_settlement_headers {
        return Err(format!(
            "settlement payload carries {} headers, exceeding MAX_SETTLEMENT_HEADERS",
            headers.len()
        ));
    }
    if headers.last() != Some(header) {
        return Err("the last header of the settlement payload is not this block".into());
    }
    if let Some(above) = headers
        .iter()
        .find(|h| h.btc_anchor.height > header.btc_anchor.height)
    {
        return Err(format!(
            "the settlement payload carries a header anchored at BTC height {}, above this block's anchor ({}); the \
             payload would never count (D-100)",
            above.btc_anchor.height, header.btc_anchor.height
        ));
    }
    if carried != entries {
        return Err("the entries of the settlement payload do not match the block contents".into());
    }
    Ok(carried_bodies(&headers, bodies))
}

/// Each body in the payload's `bodies` with the header it points at (D-077; the indices were validated on decoding).
fn carried_bodies(
    headers: &[BlockHeader],
    bodies: Vec<Body>,
) -> Vec<(BlockHeader, Vec<EntryData>)> {
    bodies
        .into_iter()
        .filter_map(|body| {
            let header = headers.get(usize::try_from(body.header).ok()?)?;
            Some((header.clone(), body.entries))
        })
        .collect()
}

/// Where the bodies a payload carries are (D-077): for each, the index of its block's header in the payload and its
/// entries. The block referred to by the last header of a settlement payload (this block) comes first, with its
/// `entries`; then the items of `bodies`. Native transaction payloads carry no bodies. A body's position in this list
/// is its slot ([`crate::tracker::LandedBody`]).
pub fn bodies_at(payload: &Payload) -> Vec<(usize, &[EntryData])> {
    let (headers, own, bodies) = match payload {
        Payload::Settlement {
            headers,
            entries,
            bodies,
        } => (headers, Some(entries), bodies),
        Payload::Headers {
            headers, bodies, ..
        } => (headers, None, bodies),
        Payload::NativeOp { .. } => return Vec::new(),
    };
    let own = own.and_then(|entries| Some((headers.len().checked_sub(1)?, entries.as_slice())));
    own.into_iter()
        .chain(bodies.iter().filter_map(|body| {
            let index = usize::try_from(body.header).ok()?;
            headers.get(index)?;
            Some((index, body.entries.as_slice()))
        }))
        .collect()
}

/// Which blocks' bodies a payload carries (D-077): (block hash, entries), in the order of [`bodies_at`].
pub fn bodies_in(payload: &Payload) -> Vec<([u8; 32], &[EntryData])> {
    let headers = match payload {
        Payload::Settlement { headers, .. } | Payload::Headers { headers, .. } => headers,
        Payload::NativeOp { .. } => return Vec::new(),
    };
    bodies_at(payload)
        .into_iter()
        .filter_map(|(index, entries)| Some((headers.get(index)?.hash(), entries)))
        .collect()
}

/// The maximum number of bytes a body occupies in any payload (D-077, D-080): the varints of the header index and the
/// entry count plus the upper bound of each entry ([`entry_data_len`]). Used to estimate the payload size in advance
/// (the budget for settlement batching and republishing) and by the consensus bound on bodies ([`check_body_fits`],
/// D-100).
pub fn body_len(entries: &[Entry]) -> Result<usize, PayloadError> {
    compact_entries::max_body_len(&entry_data(entries)).map_err(encode_err)
}

fn encode_err(e: std::io::Error) -> PayloadError {
    PayloadError::Encode(e.to_string())
}

/// The payload form of block entries.
pub fn entry_data(entries: &[Entry]) -> Vec<EntryData> {
    entries.iter().map(EntryData::from).collect()
}

/// The maximum number of bytes the varint of a count occupies (as a `u32`).
const COUNT_LEN: usize = 5;

/// The number of bytes of the payee account at the end of a republish payload (D-095).
const PAYEE_LEN: usize = 32;

/// An upper bound on the bytes of `MAX_BLOCK_DATA_BYTES` already taken by the fixed parts of a settlement payload such
/// as the headers (everything other than entries and bodies): 1 byte of variant tag, the compressed header sequence
/// (D-075, D-079), the counts of bodies and entries (varints, D-080). A header sequence that cannot be encoded (too
/// many parents) counts as `usize::MAX`. A republish payload has a payee account in place of the entries and takes
/// more: see [`headers_payload_fixed_len`].
pub fn fixed_payload_len<'a>(headers: impl IntoIterator<Item = &'a BlockHeader>) -> usize {
    compact::encoded_len(headers).map_or(usize::MAX, |len| {
        len.saturating_add(1).saturating_add(2 * COUNT_LEN)
    })
}

/// The same bound for a republish payload (`Headers`): 1 byte of variant tag, the compressed header sequence, the
/// count of bodies (varint) and the 32-byte payee account (D-095). A header sequence that cannot be encoded counts as
/// `usize::MAX`.
pub fn headers_payload_fixed_len<'a>(headers: impl IntoIterator<Item = &'a BlockHeader>) -> usize {
    compact::encoded_len(headers).map_or(usize::MAX, |len| {
        len.saturating_add(1)
            .saturating_add(COUNT_LEN)
            .saturating_add(PAYEE_LEN)
    })
}

/// The room for the entries of block `header`, by [`entry_data_len`], within which its body fits in a payload that
/// carries nothing but this block's header and body: its own settlement payload, or a republish payload (the larger
/// of the two, [`headers_payload_fixed_len`] plus the varints of the body's header index and entry count). The
/// producer adds unlock and path B entries only within it, so that every block it makes satisfies
/// [`check_body_fits`]; forced-inclusion entries are not bounded by it (§7.4), and a block with only those needs no
/// body.
pub fn body_room(params: &Params, header: &BlockHeader) -> usize {
    params
        .max_block_data_bytes
        .saturating_sub(headers_payload_fixed_len([header]))
        .saturating_sub(2 * COUNT_LEN)
}

/// The consensus bound on a block's body (D-100): a block that needs a body ([`needs_body`]) is valid only if a
/// republish payload carrying nothing but its header and its body (the payee included) fits in `MAX_BLOCK_DATA_BYTES`
/// by the upper bound every path that lands a body measures with: [`headers_payload_fixed_len`] of the header plus
/// [`body_len`], every key other than KSA counted in full as on its first occurrence (D-080). The exact encoding is
/// never longer, so anyone can then land the block's data on BTC, in a republish payload of its own or carried by any
/// payload with that much room; a body that fits only by its exact encoding, where a repeated key takes one byte, would
/// be left with no payload that carries it (second audit P4b-2). A body that cannot be encoded at all does not fit
/// either. The error describes the violation.
pub fn check_body_fits(
    params: &Params,
    header: &BlockHeader,
    entries: &[Entry],
) -> Result<(), String> {
    if !needs_body(entries) {
        return Ok(());
    }
    let data = entry_data(entries);
    Payload::Headers {
        headers: vec![header.clone()],
        bodies: vec![Body {
            header: 0,
            entries: data.clone(),
        }],
        payee: [0; PAYEE_LEN],
    }
    .encode(params)
    .map_err(|e| format!("the body cannot be encoded in a payload: {e}"))?;
    let len = compact_entries::max_body_len(&data)
        .map_err(|e| format!("the body cannot be encoded in a payload: {e}"))?
        .saturating_add(headers_payload_fixed_len([header]));
    if len > params.max_block_data_bytes {
        return Err(format!(
            "the body does not fit in a payload: a republish payload carrying only this block takes up to {len} bytes \
             with every key written in full, above MAX_BLOCK_DATA_BYTES ({})",
            params.max_block_data_bytes
        ));
    }
    Ok(())
}

/// The maximum number of bytes an entry occupies in any settlement payload (D-080: every key other than KSA is written
/// in full as on its first occurrence). The actual length in a payload depends on the keys seen before and is never
/// longer; the producer packs entries by this upper bound, so afterwards the body fits in any payload that carries it
/// (republishing included).
pub fn entry_data_len(entry: &Entry) -> Result<usize, PayloadError> {
    compact_entries::max_entry_len(&EntryData::from(entry)).map_err(encode_err)
}

/// The payload carried by the KSN1 envelope in a transaction's witness (D-081, not decoded): returned when the
/// transaction's carrier is exactly one well-formed envelope, otherwise `None`.
pub fn envelope_payload(params: &Params, tx: &bitcoin::Transaction) -> Option<Vec<u8>> {
    match crate::data::tx_data(&params.magic, tx) {
        TxData::One {
            carrier: Carrier::Input(_),
            payload,
        } => Some(payload),
        _ => None,
    }
}

/// The payload carried by a settlement transaction: the KSN1 envelope in the reveal transaction
/// ([`envelope_payload`]).
pub fn attached_payload(params: &Params, pair: &SettlementPair) -> Option<Vec<u8>> {
    envelope_payload(params, &pair.reveal)
}

/// The outpoints of the producer's (funding) inputs in a settlement transaction: all inputs of `tx` (for a settlement
/// transaction, pass the commit transaction, D-081).
pub fn funding_inputs(tx: &bitcoin::Transaction) -> Vec<([u8; 32], u32)> {
    tx.input
        .iter()
        .map(|i| {
            (
                i.previous_output.txid.to_byte_array(),
                i.previous_output.vout,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::payload_counts;
    use ksana_primitives::{BtcAnchor, BLOCK_VERSION, DEVNET};

    /// A run of headers like the empty blocks a republish carries (D-075, D-079).
    fn headers(count: u64) -> Vec<BlockHeader> {
        let mut parent = [0; 32];
        (1..=count)
            .map(|height| {
                let header = BlockHeader {
                    version: BLOCK_VERSION,
                    height,
                    parents: vec![parent],
                    btc_anchor: BtcAnchor {
                        height: 100,
                        hash: [9; 32],
                    },
                    entries_root: [3; 32],
                    state_root: [5; 32],
                    producer: [6; 32],
                    timestamp: 1_700_000_000 + height,
                    bits: 0x207f_ffff,
                    nonce: height,
                };
                parent = header.hash();
                header
            })
            .collect()
    }

    /// A republish payload never takes more than [`headers_payload_fixed_len`] plus the [`body_len`] of its bodies;
    /// the settlement bound [`fixed_payload_len`] does not cover its 32-byte payee (D-095).
    #[test]
    fn the_republish_bound_covers_the_payee() {
        for count in [1, 60, 300] {
            let headers = headers(count);
            let entries = vec![Entry::Unlock {
                height: 3,
                index: 0,
            }];
            for bodies in [Vec::new(), vec![(1, entries.clone())]] {
                let carried: usize = bodies
                    .iter()
                    .map(|(_, entries)| body_len(entries).unwrap())
                    .sum();
                let payload = Payload::Headers {
                    headers: headers.clone(),
                    bodies: bodies
                        .iter()
                        .map(|(header, entries)| Body {
                            header: *header,
                            entries: entry_data(entries),
                        })
                        .collect(),
                    payee: [7; 32],
                }
                .encode(&DEVNET)
                .unwrap();
                assert!(payload.len() <= headers_payload_fixed_len(&headers) + carried);
                assert!(payload.len() > fixed_payload_len(&headers) + carried);
            }
        }
    }

    /// A settlement payload belongs to its block only when every header it carries anchors at or below the block's
    /// anchor (D-100, second audit P4b-1): such a payload counts at any BTC height above that anchor, so it lands the
    /// block's data wherever it confirms on a chain holding the anchor.
    #[test]
    fn an_attached_payload_carries_headers_anchored_at_or_below_its_block() {
        let params = DEVNET;
        let mut chain = headers(3);
        let own = chain.pop().unwrap();
        let entries = vec![Entry::Unlock {
            height: 1,
            index: 0,
        }];
        let data = entry_data(&entries);
        let anchored = |height: u32| {
            let mut other = chain.clone();
            other[0].btc_anchor = BtcAnchor {
                height,
                hash: [8; 32],
            };
            other
        };
        let check = |carried: Vec<BlockHeader>| {
            let headers = [carried, vec![own.clone()]].concat();
            let bytes = payload(&params, headers.clone(), &entries, Vec::new()).unwrap();
            let verdict = settlement_belongs_to(&params, &bytes, &own, &data);
            (verdict, headers)
        };
        let below = own.btc_anchor.height;
        for height in [0, below - 1, below] {
            let (verdict, headers) = check(anchored(height));
            assert_eq!(verdict, Ok(Vec::new()), "{height}");
            assert!(payload_counts(&headers, below + 1));
        }
        for height in [below + 1, below + 1_000, u32::MAX] {
            let (verdict, headers) = check(anchored(height));
            let err = verdict.unwrap_err();
            assert!(err.contains("above this block's anchor"), "{err}");
            assert!(!payload_counts(&headers, below + 1));
        }
        // the block's own header alone always belongs
        assert_eq!(check(Vec::new()).0, Ok(Vec::new()));
    }

    /// Entries filling [`body_room`] fit in the block's own settlement payload and in a republish payload carrying
    /// the block's header and body.
    #[test]
    fn a_body_within_the_room_fits_in_either_payload() {
        let params = Params {
            max_block_data_bytes: 1_024,
            ..DEVNET
        };
        let mut chain = headers(2);
        let own = chain.pop().unwrap();
        let mut entries = Vec::new();
        let mut used = 0;
        for index in 0.. {
            let entry = Entry::Unlock {
                height: u64::MAX - index,
                index: u32::MAX,
            };
            let len = entry_data_len(&entry).unwrap();
            if used + len > body_room(&params, &own) {
                break;
            }
            used += len;
            entries.push(entry);
        }
        assert!(body_room(&params, &own) - used < 20);
        assert!(
            headers_payload_fixed_len([&own]) + body_len(&entries).unwrap()
                <= params.max_block_data_bytes
        );
        let settlement = payload(&params, vec![own.clone()], &entries, Vec::new()).unwrap();
        assert!(settlement.len() <= params.max_block_data_bytes);
        assert_eq!(check_body_fits(&params, &own, &entries), Ok(()));
        let republish = Payload::Headers {
            headers: vec![own],
            bodies: vec![Body {
                header: 0,
                entries: entry_data(&entries),
            }],
            payee: [7; 32],
        }
        .encode(&params)
        .unwrap();
        assert!(republish.len() <= params.max_block_data_bytes);
    }

    /// The consensus bound on bodies (D-100): a body fits exactly when a republish payload carrying only the block's
    /// header and body (with its 32-byte payee) is within `MAX_BLOCK_DATA_BYTES` by the upper bound the landing paths
    /// measure with, every key written in full; a body that fits only by the exact encoding, where a repeated key takes
    /// one byte, does not (second audit P4b-2). A block that needs no body (empty, or only forced-inclusion entries,
    /// which land as transaction IDs in the header's place) is never bound by it.
    #[test]
    fn a_body_fits_when_its_landing_bound_does() {
        use ksana_primitives::{Action, FeeMode, SigScheme, VmOp, VMOP_VERSION};
        let params = Params {
            max_block_data_bytes: 1_024,
            ..DEVNET
        };
        let own = headers(1).pop().unwrap();
        let op = |nonce: u64, fee: FeeMode| VmOp {
            version: VMOP_VERSION,
            chain_id: params.chain_id,
            account: [1; 32],
            nonce,
            fee,
            max_gas: 0,
            action: Action::Call {
                contract: [2; 32],
                method: "pad".into(),
                args: vec![0xab; 96],
                funds: Vec::new(),
            },
            sig: SigScheme::Bip340([0; 64]),
        };
        let republish = |entries: &[Entry]| {
            Payload::Headers {
                headers: vec![own.clone()],
                bodies: vec![Body {
                    header: 0,
                    entries: entry_data(entries),
                }],
                payee: [0; PAYEE_LEN],
            }
            .encode(&params)
            .unwrap()
            .len()
        };
        let bound =
            |entries: &[Entry]| headers_payload_fixed_len([&own]) + body_len(entries).unwrap();
        // path B entries of one account calling one contract, one at a time: accepted exactly up to the last one the
        // bound holds; after the first op the exact encoding refers to both keys by one byte, so some bodies fit only
        // by it, and they are refused
        let mut entries = Vec::new();
        let (mut fitted, mut only_exactly) = (0, 0);
        for nonce in 0..40 {
            entries.push(Entry::Sponsored {
                op: op(nonce, FeeMode::Token { amount: 1 }),
            });
            assert!(republish(&entries) <= bound(&entries));
            let fits = bound(&entries) <= params.max_block_data_bytes;
            assert_eq!(
                check_body_fits(&params, &own, &entries).is_ok(),
                fits,
                "{nonce}"
            );
            if fits {
                fitted += 1;
            } else if republish(&entries) <= params.max_block_data_bytes {
                only_exactly += 1;
            }
        }
        assert!(fitted > 2 && fitted < 40, "{fitted}");
        assert!(only_exactly > 0, "{only_exactly}");
        let err = check_body_fits(&params, &own, &entries).unwrap_err();
        assert!(err.contains("does not fit in a payload"), "{err}");
        // no body needed: empty, or forced-inclusion entries only, however many
        assert_eq!(check_body_fits(&params, &own, &[]), Ok(()));
        let forced: Vec<Entry> = (0..40)
            .map(|nonce| Entry::Forced {
                btc_txid: [u8::try_from(nonce).unwrap(); 32],
                op: op(nonce, FeeMode::BtcNative),
            })
            .collect();
        assert!(republish(&forced) > params.max_block_data_bytes);
        assert_eq!(check_body_fits(&params, &own, &forced), Ok(()));
        // ... until one entry that needs a body joins them: then all of it has to fit
        let mut mixed = forced.clone();
        mixed.push(Entry::Unlock {
            height: 1,
            index: 0,
        });
        assert_eq!(
            check_body_fits(&params, &own, &mixed).is_ok(),
            bound(&mixed) <= params.max_block_data_bytes
        );
    }
}
