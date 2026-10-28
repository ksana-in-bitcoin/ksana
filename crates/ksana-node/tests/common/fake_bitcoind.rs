//! Fake bitcoind: exposes the in-memory mock chain (`ksana_btc::mock::MockBtc`) over a bitcoind-compatible JSON-RPC,
//! for the node tests that run the node as a child process (the node reaches it through the real `RpcClient`), so that
//! `cargo test` does not depend on external services.
//!
//! Only the methods the node uses are implemented; the response format is the same as Bitcoin Core 28 (hashes as hex in
//! display byte order, error codes -5 / -8 / -25 / -26).

use std::net::SocketAddr;
use std::sync::Arc;

use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, OutPoint, ScriptBuf, Transaction, Txid};
use jsonrpsee::server::{RpcModule, Server, ServerHandle};
use jsonrpsee::types::ErrorObjectOwned;
use ksana_btc::client::BtcClient;
use ksana_btc::mock::MockBtc;
use serde_json::{json, Value};

/// A running fake bitcoind.
pub struct FakeBitcoind {
    pub url: String,
    pub mock: Arc<MockBtc>,
    handle: ServerHandle,
    runtime: tokio::runtime::Runtime,
}

impl Drop for FakeBitcoind {
    fn drop(&mut self) {
        let _ = self.handle.stop();
        self.runtime.block_on(self.handle.clone().stopped());
    }
}

fn err(code: i32, message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(code, message.into(), None::<()>)
}

fn block_hex(hash: &[u8; 32]) -> String {
    BlockHash::from_byte_array(*hash).to_string()
}

fn tx_hex(txid: &[u8; 32]) -> String {
    Txid::from_byte_array(*txid).to_string()
}

fn parse_block(s: &str) -> Result<[u8; 32], ErrorObjectOwned> {
    s.parse::<BlockHash>()
        .map(|h| h.to_byte_array())
        .map_err(|e| err(-8, e.to_string()))
}

fn parse_txid(s: &str) -> Result<[u8; 32], ErrorObjectOwned> {
    s.parse::<Txid>()
        .map(|h| h.to_byte_array())
        .map_err(|e| err(-8, e.to_string()))
}

fn internal(e: impl std::fmt::Display) -> ErrorObjectOwned {
    err(-1, e.to_string())
}

fn sats_to_btc(sats: u64) -> Value {
    // Display only (the node does not read floating-point amounts)
    json!(sats as f64 / 100_000_000.0)
}

/// Start on a random port of 127.0.0.1.
pub fn start(mock: Arc<MockBtc>) -> FakeBitcoind {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut module = RpcModule::new(Arc::clone(&mock));

    module
        .register_method("getblockchaininfo", |_, m, _| {
            let (height, hash) = m.tip().map_err(internal)?;
            Ok::<Value, ErrorObjectOwned>(json!({
                "chain": "regtest",
                "blocks": height,
                "headers": height,
                "bestblockhash": block_hex(&hash),
            }))
        })
        .unwrap();
    module
        .register_method("getbestblockhash", |_, m, _| {
            let (_, hash) = m.tip().map_err(internal)?;
            Ok::<Value, ErrorObjectOwned>(json!(block_hex(&hash)))
        })
        .unwrap();
    module
        .register_method("getblockcount", |_, m, _| {
            let (height, _) = m.tip().map_err(internal)?;
            Ok::<Value, ErrorObjectOwned>(json!(height))
        })
        .unwrap();
    module
        .register_method("getblockhash", |params, m, _| {
            let (height,): (u32,) = params.parse()?;
            match m.block_hash(height).map_err(internal)? {
                Some(hash) => Ok::<Value, ErrorObjectOwned>(json!(block_hex(&hash))),
                None => Err(err(-8, "Block height out of range")),
            }
        })
        .unwrap();
    module
        .register_method("getblock", |params, m, _| {
            let mut seq = params.sequence();
            let hash: String = seq.next()?;
            let verbosity: Option<u8> = seq.optional_next()?;
            let hash = parse_block(&hash)?;
            let block = m
                .block(&hash)
                .map_err(internal)?
                .ok_or_else(|| err(-5, "Block not found"))?;
            match verbosity {
                Some(0) => Ok::<Value, ErrorObjectOwned>(json!(serialize_hex(&block))),
                _ => Err(err(-8, "fake bitcoind only supports verbosity 0")),
            }
        })
        .unwrap();
    module
        .register_method("getblockheader", |params, m, _| {
            let mut seq = params.sequence();
            let hash: String = seq.next()?;
            let verbose: Option<bool> = seq.optional_next()?;
            let hash = parse_block(&hash)?;
            let info = m
                .header(&hash)
                .map_err(internal)?
                .ok_or_else(|| err(-5, "Block not found"))?;
            if verbose == Some(false) {
                return Ok::<Value, ErrorObjectOwned>(json!(serialize_hex(&info.header)));
            }
            let (tip, _) = m.tip().map_err(internal)?;
            let confirmations: i64 = if info.in_best_chain {
                i64::from(tip) - i64::from(info.height) + 1
            } else {
                -1
            };
            Ok(json!({
                "hash": block_hex(&hash),
                "confirmations": confirmations,
                "height": info.height,
                "version": info.header.version.to_consensus(),
                "merkleroot": info.header.merkle_root.to_string(),
                "time": info.header.time,
                "nonce": info.header.nonce,
                "bits": format!("{:08x}", info.header.bits.to_consensus()),
                "previousblockhash": info.header.prev_blockhash.to_string(),
            }))
        })
        .unwrap();
    module
        .register_method("getrawtransaction", |params, m, _| {
            let mut seq = params.sequence();
            let txid: String = seq.next()?;
            let verbose: Option<Value> = seq.optional_next()?;
            let txid = parse_txid(&txid)?;
            let info = m
                .transaction(&txid)
                .map_err(internal)?
                .ok_or_else(|| err(-5, "No such mempool or blockchain transaction"))?;
            let verbose = match verbose {
                Some(Value::Bool(b)) => b,
                Some(Value::Number(n)) => n.as_u64().unwrap_or(0) > 0,
                _ => false,
            };
            if !verbose {
                return Ok::<Value, ErrorObjectOwned>(json!(serialize_hex(&info.tx)));
            }
            let mut out = json!({
                "txid": tx_hex(&txid),
                "hex": serialize_hex(&info.tx),
            });
            if let Some(block) = info.block {
                let (tip, _) = m.tip().map_err(internal)?;
                let height = m
                    .header(&block)
                    .map_err(internal)?
                    .map(|h| h.height)
                    .unwrap_or(0);
                out["blockhash"] = json!(block_hex(&block));
                out["confirmations"] = json!(i64::from(tip) - i64::from(height) + 1);
            }
            Ok(out)
        })
        .unwrap();
    module
        .register_method("gettxout", |params, m, _| {
            let mut seq = params.sequence();
            let txid: String = seq.next()?;
            let vout: u32 = seq.next()?;
            let include_mempool: Option<bool> = seq.optional_next()?;
            let outpoint = OutPoint::new(Txid::from_byte_array(parse_txid(&txid)?), vout);
            let info = m
                .utxo(&outpoint, include_mempool.unwrap_or(true))
                .map_err(internal)?;
            let (_, tip) = m.tip().map_err(internal)?;
            Ok::<Value, ErrorObjectOwned>(match info {
                None => Value::Null,
                Some(u) => json!({
                    "bestblock": block_hex(&tip),
                    "confirmations": u.confirmations,
                    "value": sats_to_btc(u.txout.value.to_sat()),
                    "scriptPubKey": { "hex": hex::encode(u.txout.script_pubkey.as_bytes()) },
                    "coinbase": u.coinbase,
                }),
            })
        })
        .unwrap();
    module
        .register_method("scantxoutset", |params, m, _| {
            let mut seq = params.sequence();
            let action: String = seq.next()?;
            if action != "start" {
                return Err(err(-8, "fake bitcoind only supports start"));
            }
            let scan: Vec<Value> = seq.next()?;
            let mut unspents = Vec::new();
            for object in scan {
                let desc = object["desc"]
                    .as_str()
                    .or_else(|| object.as_str())
                    .ok_or_else(|| err(-8, "missing desc"))?;
                let raw = desc
                    .split('#')
                    .next()
                    .and_then(|d| d.strip_prefix("raw("))
                    .and_then(|d| d.strip_suffix(')'))
                    .ok_or_else(|| {
                        err(
                            -8,
                            format!("fake bitcoind only supports raw() descriptors: {desc}"),
                        )
                    })?;
                let script = ScriptBuf::from_bytes(hex::decode(raw).map_err(internal)?);
                for u in m.scan_utxos(&script).map_err(internal)? {
                    unspents.push(json!({
                        "txid": u.outpoint.txid.to_string(),
                        "vout": u.outpoint.vout,
                        "scriptPubKey": hex::encode(u.txout.script_pubkey.as_bytes()),
                        "desc": desc,
                        "amount": sats_to_btc(u.txout.value.to_sat()),
                        "coinbase": u.coinbase,
                        "height": u.height,
                    }));
                }
            }
            let (height, hash) = m.tip().map_err(internal)?;
            Ok::<Value, ErrorObjectOwned>(json!({
                "success": true,
                "txouts": unspents.len(),
                "height": height,
                "bestblock": block_hex(&hash),
                "unspents": unspents,
            }))
        })
        .unwrap();
    module
        .register_method("testmempoolaccept", |params, m, _| {
            let mut seq = params.sequence();
            let raws: Vec<String> = seq.next()?;
            let txs = raws
                .iter()
                .map(|raw| deserialize_hex::<Transaction>(raw))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| err(-22, e.to_string()))?;
            // Checked as one package (parent transactions first); when rejected, every transaction reports the same
            // reason
            let refs: Vec<&Transaction> = txs.iter().collect();
            let verdict = m.test_accept(&refs).map_err(internal)?;
            let results: Vec<Value> = txs
                .iter()
                .map(|tx| {
                    let txid = tx.compute_txid().to_string();
                    match &verdict {
                        Ok(()) => json!({ "txid": txid, "allowed": true }),
                        Err(reason) => {
                            json!({ "txid": txid, "allowed": false, "reject-reason": reason })
                        }
                    }
                })
                .collect();
            Ok::<Value, ErrorObjectOwned>(json!(results))
        })
        .unwrap();
    module
        .register_method("sendrawtransaction", |params, m, _| {
            let mut seq = params.sequence();
            let raw: String = seq.next()?;
            let tx: Transaction = deserialize_hex(&raw).map_err(|e| err(-22, e.to_string()))?;
            match m.broadcast(&tx).map_err(internal)? {
                Ok(()) => Ok::<Value, ErrorObjectOwned>(json!(tx.compute_txid().to_string())),
                Err(reason) => Err(err(-26, reason)),
            }
        })
        .unwrap();
    module
        .register_method("getmempoolentry", |params, m, _| {
            let (txid,): (String,) = params.parse()?;
            if m.in_mempool(&parse_txid(&txid)?).map_err(internal)? {
                Ok::<Value, ErrorObjectOwned>(
                    json!({ "vsize": 0, "ancestorcount": 1, "descendantcount": 1 }),
                )
            } else {
                Err(err(-5, "Transaction not in mempool"))
            }
        })
        .unwrap();
    module
        .register_method("getindexinfo", |_, m, _| {
            let (height, _) = m.tip().map_err(internal)?;
            Ok::<Value, ErrorObjectOwned>(json!({
                "txindex": { "synced": true, "best_block_height": height }
            }))
        })
        .unwrap();

    let (handle, addr): (ServerHandle, SocketAddr) = runtime.block_on(async {
        let server = Server::builder()
            .build("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        (server.start(module), addr)
    });
    FakeBitcoind {
        url: format!("http://{addr}"),
        mock,
        handle,
        runtime,
    }
}
