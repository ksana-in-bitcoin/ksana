//! proptest strategies: generate arbitrary values of every consensus type (tests only; public when the `proptest`
//! feature is enabled).

use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;

use crate::params::DEVNET;
use crate::types::{
    Action, Block, BlockHeader, Body, BtcAnchor, Entry, EntryData, FeeMode, Funds, OutPoint,
    Payload, SettlementTxs, SigScheme, VmOp,
};

/// An arbitrary byte string (length 0..64).
pub fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    vec(any::<u8>(), 0..64)
}

/// Arbitrary 32 bytes.
pub fn bytes32() -> impl Strategy<Value = [u8; 32]> {
    any::<[u8; 32]>()
}

/// A 32-byte key (account, contract, asset, recipient): half of them drawn from four values including the KSA asset
/// ID, so that key dictionary references in payloads are common (D-080).
pub fn key() -> impl Strategy<Value = [u8; 32]> {
    prop_oneof![bytes32(), (0u8..4).prop_map(|i| [i; 32])]
}

/// An arbitrary [`OutPoint`].
pub fn outpoint() -> impl Strategy<Value = OutPoint> {
    (bytes32(), any::<u32>()).prop_map(|(txid, vout)| OutPoint { txid, vout })
}

/// An arbitrary [`FeeMode`].
pub fn fee_mode() -> impl Strategy<Value = FeeMode> {
    prop_oneof![
        any::<u128>().prop_map(|amount| FeeMode::Token { amount }),
        Just(FeeMode::BtcNative),
    ]
}

/// An arbitrary attached funds list (0-3 items, neither ascending order nor non-zero guaranteed: the codec layer does
/// not check those execution rules).
pub fn funds() -> impl Strategy<Value = Vec<Funds>> {
    vec(
        (key(), any::<u128>()).prop_map(|(asset, amount)| Funds { asset, amount }),
        0..3,
    )
}

/// An arbitrary [`Action`] (the method name is an arbitrary Unicode string).
pub fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        (small_bytes(), small_bytes(), funds()).prop_map(|(code, init_args, funds)| {
            Action::Deploy {
                code,
                init_args,
                funds,
            }
        }),
        (key(), any::<String>(), small_bytes(), funds()).prop_map(
            |(contract, method, args, funds)| Action::Call {
                contract,
                method,
                args,
                funds,
            }
        ),
        (key(), key(), any::<u128>()).prop_map(|(asset, to, amount)| Action::AssetTransfer {
            asset,
            to,
            amount
        }),
    ]
}

/// An arbitrary [`SigScheme`].
pub fn sig_scheme() -> impl Strategy<Value = SigScheme> {
    prop_oneof![
        any::<[u8; 64]>().prop_map(SigScheme::Bip340),
        small_bytes().prop_map(SigScheme::Bip322Simple),
    ]
}

/// An arbitrary [`VmOp`] (field values are not constrained by protocol validity).
pub fn vmop() -> impl Strategy<Value = VmOp> {
    (
        any::<u8>(),
        any::<u32>(),
        key(),
        any::<u64>(),
        fee_mode(),
        any::<u64>(),
        action(),
        sig_scheme(),
    )
        .prop_map(
            |(version, chain_id, account, nonce, fee, max_gas, action, sig)| VmOp {
                version,
                chain_id,
                account,
                nonce,
                fee,
                max_gas,
                action,
                sig,
            },
        )
}

/// An arbitrary [`BtcAnchor`].
pub fn btc_anchor() -> impl Strategy<Value = BtcAnchor> {
    (any::<u32>(), bytes32()).prop_map(|(height, hash)| BtcAnchor { height, hash })
}

/// An arbitrary [`BlockHeader`] (0-3 parents).
pub fn block_header() -> impl Strategy<Value = BlockHeader> {
    (
        any::<u32>(),
        any::<u64>(),
        proptest::collection::vec(bytes32(), 0..4),
        btc_anchor(),
        bytes32(),
        bytes32(),
        bytes32(),
        any::<u64>(),
        any::<u32>(),
        any::<u64>(),
    )
        .prop_map(
            |(
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
            )| BlockHeader {
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
            },
        )
}

/// An arbitrary [`Entry`].
pub fn entry() -> impl Strategy<Value = Entry> {
    prop_oneof![
        (bytes32(), vmop()).prop_map(|(btc_txid, op)| Entry::Forced { btc_txid, op }),
        vmop().prop_map(|op| Entry::Sponsored { op }),
        any::<(u64, u32)>().prop_map(|(height, index)| Entry::Unlock { height, index }),
    ]
}

/// An arbitrary [`EntryData`].
pub fn entry_data() -> impl Strategy<Value = EntryData> {
    prop_oneof![
        bytes32().prop_map(|btc_txid| EntryData::Forced { btc_txid }),
        vmop().prop_map(|op| EntryData::Sponsored { op }),
        any::<(u64, u32)>().prop_map(|(height, index)| EntryData::Unlock { height, index }),
    ]
}

/// An arbitrary [`Block`].
pub fn block() -> impl Strategy<Value = Block> {
    (
        block_header(),
        vec(entry(), 0..4),
        option::of((small_bytes(), small_bytes())),
    )
        .prop_map(|(header, entries, settlement)| Block {
            header,
            entries,
            settlement: settlement.map(|(commit, reveal)| SettlementTxs { commit, reveal }),
        })
}

/// An entry other than forced inclusions in a body (D-077).
fn body_entry_data() -> impl Strategy<Value = EntryData> {
    prop_oneof![
        vmop().prop_map(|op| EntryData::Sponsored { op }),
        any::<(u64, u32)>().prop_map(|(height, index)| EntryData::Unlock { height, index }),
    ]
}

/// A canonical body (D-077): forced-inclusion entries first, followed by at least one other entry.
fn body_entries() -> impl Strategy<Value = Vec<EntryData>> {
    (
        vec(
            bytes32().prop_map(|btc_txid| EntryData::Forced { btc_txid }),
            0..2,
        ),
        vec(body_entry_data(), 1..3),
    )
        .prop_map(|(mut forced, rest)| {
            forced.extend(rest);
            forced
        })
}

/// Headers and bodies pointing at some of them (indices strictly increasing; never at the last one when
/// `settlement`).
fn headers_and_bodies(settlement: bool) -> impl Strategy<Value = (Vec<BlockHeader>, Vec<Body>)> {
    (
        vec(block_header(), 0..4),
        vec(option::of(body_entries()), 4),
    )
        .prop_map(move |(headers, picks)| {
            let limit = if settlement {
                headers.len().saturating_sub(1)
            } else {
                headers.len()
            };
            let bodies = picks
                .into_iter()
                .take(limit)
                .enumerate()
                .filter_map(|(index, entries)| {
                    Some(Body {
                        header: u32::try_from(index).ok()?,
                        entries: entries?,
                    })
                })
                .collect();
            (headers, bodies)
        })
}

/// Entries in a payload: the VmOp chain ID is the network's chain ID (D-080: `Settlement` and `Headers` payloads do
/// not write the chain ID, it is taken from the network params when decoding).
fn on_devnet(entries: Vec<EntryData>) -> Vec<EntryData> {
    entries
        .into_iter()
        .map(|entry| match entry {
            EntryData::Sponsored { mut op } => {
                op.chain_id = DEVNET.chain_id;
                EntryData::Sponsored { op }
            }
            other => other,
        })
        .collect()
}

fn bodies_on_devnet(bodies: Vec<Body>) -> Vec<Body> {
    bodies
        .into_iter()
        .map(|body| Body {
            header: body.header,
            entries: on_devnet(body.entries),
        })
        .collect()
}

/// An arbitrary [`Payload`] (the VmOps in `Settlement` and `Headers` are on devnet, see [`Payload::encode`]).
pub fn payload() -> impl Strategy<Value = Payload> {
    prop_oneof![
        (headers_and_bodies(true), vec(entry_data(), 0..4)).prop_map(
            |((headers, bodies), entries)| Payload::Settlement {
                headers,
                entries: on_devnet(entries),
                bodies: bodies_on_devnet(bodies),
            }
        ),
        vmop().prop_map(|op| Payload::NativeOp { op }),
        (headers_and_bodies(false), any::<[u8; 32]>()).prop_map(|((headers, bodies), payee)| {
            Payload::Headers {
                headers,
                bodies: bodies_on_devnet(bodies),
                payee,
            }
        }),
    ]
}
