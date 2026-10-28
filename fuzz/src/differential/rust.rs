//! The Rust side: what the node's code makes of each [`Request`], described as JSON in the same form the oracle
//! describes the TypeScript side's answer.

use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash;
use bitcoin::{Script, Transaction, Witness, XOnlyPublicKey};
use ksana_btc::data::{
    data_script, is_ksn1_output, parse_data_script, tx_data, tx_payload, Carrier, TxData,
};
use ksana_btc::envelope::{envelope_script, is_ksn1_envelope, parse_envelope, witness_leaf};
use ksana_btc::native::{first_input, native_op};
use ksana_consensus::{check_pow, effective_target, target_from_bits, Argon2id, ConsensusError};
use ksana_contract_sdk::{ks20, ks721};
use ksana_crypto::bip322::{verify_for_account, verify_simple};
use ksana_crypto::{
    account_address, account_from_address, entries_root, merkle_root, native_vmop_sighash,
    parse_account, sign_native_vmop, sign_native_vmop_bip322_p2tr, sign_vmop,
    sign_vmop_bip322_p2tr, verify_native_vmop, verify_vmop, vmop_sighash,
};
use ksana_primitives::codec::decode;
use ksana_primitives::{
    Action, Block, BlockHeader, BtcAnchor, Entry, EntryData, FeeMode, Funds, Network, OutPoint,
    Payload, PowAlgorithmId, SigScheme, VmOp, DEVNET,
};
use serde::Serialize;
use serde_json::{json, Value};

use super::abi::{decode_abi_event, ABI_TYPES};
use super::request::{BorshType, BtcNet, Request, Signing, FIRST_INPUT};

/// The length limit of [`Request::Borsh`] on both sides.
pub const MAX_BORSH_BYTES: usize = 1 << 20;

/// The key [`Request::Write`] writes envelopes with: the x coordinate of the curve's generator, a valid x-only key.
pub const WRITE_KEY: [u8; 32] = [
    0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b, 0x07,
    0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8, 0x17, 0x98,
];

fn hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(bytes)
}

fn refused() -> Value {
    json!({ "refused": true })
}

fn to_json(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// What the Rust side makes of `request`; `None` when the request is outside what the two sides must agree on (see
/// the module documentation).
pub fn describe(request: &Request) -> Option<Value> {
    Some(match request {
        Request::VmOp { bytes, sign } => vmop(bytes, sign.as_ref()),
        Request::Payload { network, bytes } => match Payload::decode(bytes, network.params()) {
            Ok(payload) => json!({ "value": to_json(&payload) }),
            Err(_) => refused(),
        },
        Request::Borsh { ty, bytes } => borsh(*ty, bytes),
        Request::Tx { network, bytes } => return tx(*network, bytes),
        Request::Script { script } => script_json(script),
        Request::Witness { items } => {
            let witness = Witness::from_slice(items);
            json!({ "leaf": witness_leaf(&witness).map(|s| hex(s.as_bytes())) })
        }
        Request::Write { payload } => write(payload),
        Request::Address { network, text } => address(*network, text),
        Request::Account { network, account } => json!({
            "address": account_address(account, network.bitcoin()).ok().map(|a| a.to_string()),
        }),
        Request::Bip322 {
            challenge,
            account,
            message,
            signature,
        } => json!({
            "simple": verify_simple(Script::from_bytes(challenge), message, signature).is_ok(),
            "for_account": verify_for_account(account, message, signature).is_ok(),
        }),
        Request::Bits {
            bits,
            pow_limit_bits,
            weight,
        } => json!({
            "target": target_from_bits(*bits, *pow_limit_bits).ok().map(|t| t.to_hex()),
            "effective": effective_target(*bits, *pow_limit_bits, *weight).ok().map(|t| t.to_hex()),
        }),
        Request::Pow {
            network,
            header,
            weight,
        } => pow(*network, header, *weight),
        Request::Rewards {
            network,
            daa_score,
            weight,
            amount,
        } => rewards(*network, *daa_score, *weight, *amount),
        Request::Event { standards, data } => return event(standards, data),
        Request::Abi { ty, bytes } => match ABI_TYPES.get(*ty)?.decode(bytes) {
            // A value the contract SDK accepts always writes back to the same bytes (Borsh is canonical)
            Some(value) => json!({ "value": value, "canonical": true }),
            None => refused(),
        },
        Request::AbiEvent { bytes } => decode_abi_event(bytes).unwrap_or_else(refused),
        Request::Merkle { leaves } => json!({ "root": hex(merkle_root(leaves)) }),
        Request::Params { network } => params(*network),
    })
}

fn signature_hex(sig: Result<SigScheme, ksana_crypto::SigError>) -> Option<String> {
    match sig.ok()? {
        SigScheme::Bip340(bytes) => Some(hex(bytes)),
        SigScheme::Bip322Simple(bytes) => Some(hex(bytes)),
    }
}

fn vmop(bytes: &[u8], sign: Option<&Signing>) -> Value {
    let Ok(op) = VmOp::decode(bytes, &DEVNET) else {
        return refused();
    };
    let mut out = json!({
        "value": to_json(&op),
        "op_id": op.op_id().ok().map(hex),
        "sighash": vmop_sighash(&op).ok().map(hex),
        "native_sighash": native_vmop_sighash(&op, &FIRST_INPUT).ok().map(hex),
        "verify": verify_vmop(&op).is_ok(),
        "verify_native": verify_native_vmop(&op, &FIRST_INPUT).is_ok(),
    });
    if let Some(sign) = sign {
        let (bip340, bip322) = match ksana_crypto::keypair_from_secret(&sign.secret) {
            Err(_) => (None, None),
            Ok(keypair) => match op.fee {
                FeeMode::Token { .. } => (
                    signature_hex(sign_vmop(&op, &keypair, &sign.aux)),
                    signature_hex(sign_vmop_bip322_p2tr(&op, &keypair, &sign.aux)),
                ),
                FeeMode::BtcNative => (
                    signature_hex(sign_native_vmop(&op, &FIRST_INPUT, &keypair, &sign.aux)),
                    signature_hex(sign_native_vmop_bip322_p2tr(
                        &op,
                        &FIRST_INPUT,
                        &keypair,
                        &sign.aux,
                    )),
                ),
            },
        };
        out["signatures"] = json!({ "bip340": bip340, "bip322": bip322 });
    }
    out
}

fn value<T: borsh::BorshDeserialize + Serialize>(bytes: &[u8]) -> Result<(T, Value), Value> {
    let decoded = decode::<T>(bytes, MAX_BORSH_BYTES).map_err(|_| refused())?;
    let json = to_json(&decoded);
    Ok((decoded, json))
}

fn borsh(ty: BorshType, bytes: &[u8]) -> Value {
    let described = match ty {
        BorshType::OutPoint => value::<OutPoint>(bytes).map(|(_, v)| json!({ "value": v })),
        BorshType::FeeMode => value::<FeeMode>(bytes).map(|(_, v)| json!({ "value": v })),
        BorshType::Funds => value::<Funds>(bytes).map(|(_, v)| json!({ "value": v })),
        BorshType::Action => value::<Action>(bytes).map(|(_, v)| json!({ "value": v })),
        BorshType::SigScheme => value::<SigScheme>(bytes).map(|(_, v)| json!({ "value": v })),
        BorshType::VmOp => value::<VmOp>(bytes)
            .map(|(op, v)| json!({ "value": v, "op_id": op.op_id().ok().map(hex) })),
        BorshType::BtcAnchor => value::<BtcAnchor>(bytes).map(|(_, v)| json!({ "value": v })),
        BorshType::BlockHeader => value::<BlockHeader>(bytes)
            .map(|(header, v)| json!({ "value": v, "hash": hex(header.hash()) })),
        BorshType::Entry => value::<Entry>(bytes).map(|(_, v)| json!({ "value": v })),
        BorshType::EntryData => value::<EntryData>(bytes).map(|(_, v)| json!({ "value": v })),
        BorshType::Block => value::<Block>(bytes).map(|(block, v)| {
            json!({
                "value": v,
                "hash": hex(block.header.hash()),
                "entries_root": entries_root(&block.entries).ok().map(hex),
            })
        }),
    };
    described.unwrap_or_else(|refused| refused)
}

fn carrier_json(carrier: Carrier) -> Value {
    match carrier {
        Carrier::Output(index) => json!({ "kind": "Output", "index": index }),
        Carrier::Input(index) => json!({ "kind": "Input", "index": index }),
    }
}

fn tx(network: Network, bytes: &[u8]) -> Option<Value> {
    let Ok(tx) = deserialize::<Transaction>(bytes) else {
        return Some(refused());
    };
    // Consensus forbids a transaction without inputs; rust-bitcoin reads a segwit-marked one that Bitcoin Core and
    // the SDK refuse
    if tx.input.is_empty() {
        return None;
    }
    let params = network.params();
    let data = match tx_data(&params.magic, &tx) {
        TxData::None => json!("none"),
        TxData::Invalid => json!("invalid"),
        TxData::One { carrier, payload } => {
            json!({ "carrier": carrier_json(carrier), "payload": hex(payload) })
        }
    };
    let inputs: Vec<Value> = tx
        .input
        .iter()
        .map(|input| {
            json!({
                "txid": hex(input.previous_output.txid.to_byte_array()),
                "vout": input.previous_output.vout,
                "script_sig": hex(input.script_sig.as_bytes()),
                "sequence": input.sequence.0,
                "witness": input.witness.iter().map(hex).collect::<Vec<_>>(),
            })
        })
        .collect();
    let outputs: Vec<Value> = tx
        .output
        .iter()
        .map(|output| {
            json!({
                "value": output.value.to_sat().to_string(),
                "script": hex(output.script_pubkey.as_bytes()),
            })
        })
        .collect();
    Some(json!({
        "tx": {
            "txid": hex(tx.compute_txid().to_byte_array()),
            "version": tx.version.0,
            "lock_time": tx.lock_time.to_consensus_u32(),
            "inputs": inputs,
            "outputs": outputs,
            "bytes": hex(serialize(&tx)),
        },
        "first_input": first_input(&tx).map(|o| json!({ "txid": hex(o.txid), "vout": o.vout })),
        "data": data,
        "payload": tx_payload(params, &tx)
            .map(|(carrier, payload)| json!({ "carrier": carrier_json(carrier), "payload": to_json(&payload) })),
        "native_op": native_op(params, &tx).map(|op| to_json(&op)),
    }))
}

fn script_json(script: &[u8]) -> Value {
    let magic = &DEVNET.magic;
    let script = Script::from_bytes(script);
    let envelope = parse_envelope(magic, script);
    json!({
        "output": {
            "is_ksn1": is_ksn1_output(magic, script),
            "payload": parse_data_script(magic, script).map(hex),
        },
        "envelope": {
            "is_ksn1": is_ksn1_envelope(magic, script),
            "key": envelope.as_ref().map(|(key, _)| hex(key)),
            "payload": envelope.map(|(_, payload)| hex(payload)),
        },
    })
}

fn write(payload: &[u8]) -> Value {
    let magic = &DEVNET.magic;
    let envelope = XOnlyPublicKey::from_slice(&WRITE_KEY)
        .ok()
        .and_then(|key| envelope_script(&key, magic, payload).ok());
    json!({
        "data_script": data_script(magic, payload).ok().map(|s| hex(s.as_bytes())),
        "envelope_script": envelope.map(|s| hex(s.as_bytes())),
    })
}

fn address(network: BtcNet, text: &str) -> Value {
    json!({
        "from_address": account_from_address(text, network.bitcoin()).ok().map(hex),
        "parse_account": parse_account(text, network.bitcoin()).ok().map(hex),
    })
}

fn pow(network: Network, header: &[u8], weight: u64) -> Value {
    let Ok(header) = decode::<BlockHeader>(header, MAX_BORSH_BYTES) else {
        return refused();
    };
    let params = network.params();
    let PowAlgorithmId::Argon2id(argon) = params.pow_algorithm;
    let Ok(algorithm) = Argon2id::new(params.chain_id, argon) else {
        return json!({ "algorithm": "unavailable" });
    };
    let check = match check_pow(&algorithm, &header, params.pow_limit_bits, weight) {
        Ok(()) => "ok",
        Err(ConsensusError::InsufficientPow) => "insufficient",
        Err(ConsensusError::PowUnavailable(_)) => "unavailable",
        Err(_) => "invalid",
    };
    json!({ "hash": algorithm.pow_hash(&header).ok().map(hex), "check": check })
}

fn rewards(network: Network, daa_score: u64, weight: u64, amount: u128) -> Value {
    let p = network.params();
    let rules = p.rules(daa_score);
    let split = p.block_rewards_split_at(daa_score, weight);
    json!({
        "period": p.reward_period_at(daa_score).map(|v| v.to_string()),
        "reward": p.block_reward_at(daa_score).to_string(),
        "share": p.block_share_at(daa_score).to_string(),
        "carrier": p.block_carrier_at(daa_score).to_string(),
        "carrier_fee": p.carrier_fee_at(daa_score, amount).to_string(),
        "split": {
            "producer": split.producer.to_string(),
            "share": split.share.to_string(),
            "carrier": split.carrier.to_string(),
        },
        "max_total": p.max_total_rewards().to_string(),
        "rules": {
            "version": rules.version.to_string(),
            "block_gas_limit": rules.block_gas_limit.to_string(),
            "native_max_gas": rules.native_max_gas.to_string(),
            "min_vmop_version": rules.min_vmop_version.to_string(),
            "max_vmop_version": rules.max_vmop_version.to_string(),
            "block_share_bps": rules.block_share_bps.to_string(),
            "block_share_end": rules.block_share_end.to_string(),
            "carrier_bps": rules.carrier_bps.to_string(),
            "carrier_fee_bps": rules.carrier_fee_bps.to_string(),
        },
    })
}

/// The standard a declaration names, as the node's index decides it (`Standard::of` in
/// `crates/ksana-node/src/standards.rs`, which this crate does not build): KS-20 or KS-721 first, and the other
/// nowhere.
fn standard_of(ids: &[String]) -> Option<&'static str> {
    let (standard, other) = match ids.first()?.as_str() {
        "KS-20" => ("KS-20", "KS-721"),
        "KS-721" => ("KS-721", "KS-20"),
        _ => return None,
    };
    (!ids.iter().any(|id| id == other)).then_some(standard)
}

fn opt_hex(value: &Option<[u8; 32]>) -> Value {
    value.map_or(Value::Null, |v| Value::from(hex(v)))
}

fn opt_dec(value: &Option<u64>) -> Value {
    value.map_or(Value::Null, |v| Value::from(v.to_string()))
}

fn ks20_json(event: &ks20::Event) -> Value {
    match event {
        ks20::Event::Standards(ids) => json!({ "Standards": ids }),
        ks20::Event::Metadata {
            name,
            symbol,
            decimals,
        } => json!({ "Metadata": { "name": name, "symbol": symbol, "decimals": decimals } }),
        ks20::Event::Transfer { from, to, amount } => json!({
            "Transfer": { "from": hex(from), "to": hex(to), "amount": amount.to_string() },
        }),
        ks20::Event::Approve {
            owner,
            spender,
            amount,
            expires_at,
        } => json!({
            "Approve": {
                "owner": hex(owner),
                "spender": hex(spender),
                "amount": amount.to_string(),
                "expires_at": opt_dec(expires_at),
            },
        }),
        ks20::Event::Minter { previous, minter } => json!({
            "Minter": { "previous": opt_hex(previous), "minter": opt_hex(minter) },
        }),
    }
}

fn ks721_json(event: &ks721::Event) -> Value {
    match event {
        ks721::Event::Standards(ids) => json!({ "Standards": ids }),
        ks721::Event::Metadata { name, symbol } => {
            json!({ "Metadata": { "name": name, "symbol": symbol } })
        }
        ks721::Event::Transfer { from, to, token_id } => json!({
            "Transfer": { "from": hex(from), "to": hex(to), "token_id": hex(token_id) },
        }),
        ks721::Event::Approve {
            owner,
            spender,
            token_id,
        } => json!({
            "Approve": { "owner": hex(owner), "spender": opt_hex(spender), "token_id": hex(token_id) },
        }),
        ks721::Event::Operator {
            owner,
            operator,
            approved,
            expires_at,
        } => json!({
            "Operator": {
                "owner": hex(owner),
                "operator": hex(operator),
                "approved": approved,
                "expires_at": opt_dec(expires_at),
            },
        }),
        ks721::Event::Uri(token_id) => json!({ "Uri": opt_hex(token_id) }),
    }
}

fn event(standards: &[String], data: &[u8]) -> Option<Value> {
    // A contract cannot emit a longer event; the SDK refuses one within this limit
    if data.len() > ksana_vm::limits::MAX_EVENT_BYTES {
        return None;
    }
    Some(json!({
        "ks20": ks20::Event::parse(data).as_ref().map(ks20_json),
        "ks721": ks721::Event::parse(data).as_ref().map(ks721_json),
        "standard": standard_of(standards),
    }))
}

fn params(network: Network) -> Value {
    let p = network.params();
    let PowAlgorithmId::Argon2id(argon) = p.pow_algorithm;
    let upgrades: Vec<Value> = p
        .upgrades
        .iter()
        .map(|u| {
            json!({
                "version": u.version.to_string(),
                "daa_score": u.daa_score.to_string(),
                "block_gas_limit": u.block_gas_limit.map(|v| v.to_string()),
                "native_max_gas": u.native_max_gas.map(|v| v.to_string()),
                "min_vmop_version": u.min_vmop_version.map(|v| v.to_string()),
                "max_vmop_version": u.max_vmop_version.map(|v| v.to_string()),
                "block_share_bps": u.block_share_bps.map(|v| v.to_string()),
                "block_share_end": u.block_share_end.map(|v| v.to_string()),
                "carrier_bps": u.carrier_bps.map(|v| v.to_string()),
                "carrier_fee_bps": u.carrier_fee_bps.map(|v| v.to_string()),
            })
        })
        .collect();
    // The fields the SDK's `Params` has (the others are the node's alone)
    json!({
        "chain_id": p.chain_id.to_string(),
        "magic": hex(p.magic),
        "target_block_time_secs": p.target_block_time_secs.to_string(),
        "asert_half_life_secs": p.asert_half_life_secs.to_string(),
        "settlement_depth": p.settlement_depth.to_string(),
        "settlement_timeout": p.settlement_timeout.to_string(),
        "native_confirmations": p.native_confirmations.to_string(),
        "max_anchor_lag": p.max_anchor_lag.to_string(),
        "btc_query_depth": p.btc_query_depth.to_string(),
        "btc_finality_confs": p.btc_finality_confs.to_string(),
        "min_settlement_feerate_sat_vb": p.min_settlement_feerate_sat_vb.to_string(),
        "block_gas_limit": p.block_gas_limit.to_string(),
        "native_max_gas": p.native_max_gas.to_string(),
        "max_op_bytes": p.max_op_bytes.to_string(),
        "max_block_data_bytes": p.max_block_data_bytes.to_string(),
        "max_settlement_headers": p.max_settlement_headers.to_string(),
        "max_wasm_memory_bytes": p.max_wasm_memory_bytes.to_string(),
        "max_call_depth": p.max_call_depth.to_string(),
        "reorg_window": p.reorg_window.to_string(),
        "median_time_span": p.median_time_span.to_string(),
        "max_future_block_time_secs": p.max_future_block_time_secs.to_string(),
        "pow_limit_bits": p.pow_limit_bits.to_string(),
        "pow_algorithm": { "argon2id": {
            "memory_kib": argon.memory_kib.to_string(),
            "iterations": argon.iterations.to_string(),
            "lanes": argon.lanes.to_string(),
        } },
        "block_reward": p.block_reward.to_string(),
        "reward_halving_interval": p.reward_halving_interval.to_string(),
        "reward_interval_doublings": p.reward_interval_doublings.to_string(),
        "ksa_max_supply": p.ksa_max_supply.to_string(),
        "block_share_bps": p.block_share_bps.to_string(),
        "block_share_end": p.block_share_end.to_string(),
        "carrier_bps": p.carrier_bps.to_string(),
        "carrier_fee_bps": p.carrier_fee_bps.to_string(),
        "carrier_repost_delay": p.carrier_repost_delay.to_string(),
        "upgrades": upgrades,
    })
}
