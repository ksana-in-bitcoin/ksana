//! The bitcoind access interface [`BtcClient`] and its implementation [`RpcClient`] based on the `corepc-client`
//! synchronous client.
//!
//! The node tracks the BTC chain through [`BtcClient`] (anchoring, native transaction scanning, funding outputs,
//! confirmation of settlement transactions) and submits settlement transactions; unit tests use `mock::MockBtc`
//! (feature `mock`) in place of bitcoind.
//!
//! - Transaction IDs and block hashes are always in Bitcoin's internal byte order (D-001); the hex strings in the RPC
//!   are its reverse and are converted only inside this module.
//! - [`BtcError`] only denotes a local data source fault (RPC unavailable, response unparseable); the caller must
//!   never judge a block invalid because of it (D-035); "does not exist" is expressed as `Ok(None)`.
//! - No floating-point arithmetic: output amounts are always taken from the raw transaction; the `amount` / `value`
//!   fields in BTC units in the RPC are not read.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::consensus::Decodable;
use bitcoin::hashes::Hash;
use bitcoin::{Block, OutPoint, Script, ScriptBuf, Transaction, TxOut, Txid};
use corepc_client::client_sync::v28::Client;
use corepc_client::client_sync::{Auth, Error as CorepcError};
use serde_json::{json, Value};

/// A local BTC data source fault (RPC unavailable, response unparseable). A block must never be judged invalid because
/// of it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BtcError {
    /// The call failed: a connection or HTTP error, or bitcoind returned an error other than "does not exist".
    #[error("bitcoind RPC error: {0}")]
    Rpc(String),
    /// The response is missing a field, is malformed, or does not match the request (e.g. the returned block hash is
    /// not the requested one).
    #[error("bitcoind response cannot be parsed: {0}")]
    Decode(String),
}

/// A block header and its position on the local BTC chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderInfo {
    /// The block height.
    pub height: u32,
    /// Whether it is on the current best chain.
    pub in_best_chain: bool,
    /// The 80-byte block header.
    pub header: bitcoin::block::Header,
}

/// A transaction and where it is confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxInfo {
    /// The transaction.
    pub tx: bitcoin::Transaction,
    /// The hash of the best-chain block containing it; `None` when it is in the mempool (unconfirmed).
    pub block: Option<[u8; 32]>,
}

/// An unspent output (`gettxout`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtxoInfo {
    /// The output (taken from the raw transaction).
    pub txout: bitcoin::TxOut,
    /// Confirmations; 0 for an output in the mempool.
    pub confirmations: u32,
    /// Whether it is a coinbase output.
    pub coinbase: bool,
}

/// One confirmed output found by a UTXO set scan (`scantxoutset`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedUtxo {
    /// The output's position.
    pub outpoint: bitcoin::OutPoint,
    /// The output (taken from the raw transaction).
    pub txout: bitcoin::TxOut,
    /// The height of the block it is in.
    pub height: u32,
    /// Whether it is a coinbase output.
    pub coinbase: bool,
}

/// What bitcoind's mempool holds now (`getmempoolinfo`). Only the node's local fee policy reads it (D-117), never
/// consensus code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MempoolInfo {
    /// The virtual size of all the transactions in the mempool (vB, `bytes`).
    pub vsize: u64,
    /// The lowest fee rate the mempool takes a transaction at now (sat/vB rounded up, `mempoolminfee`).
    pub min_feerate: u64,
}

/// All of the node's access to bitcoind. Implementations must be shareable between threads.
///
/// All hashes are in Bitcoin's internal byte order (D-001). `Err` only denotes a local data source fault, to be
/// retried later; a block must never be judged invalid because of it.
pub trait BtcClient: Send + Sync {
    /// The current best-chain tip (height, block hash).
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError>;
    /// The block hash at that height on the best chain; None above the tip.
    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError>;
    /// The full block; None when unknown.
    fn block(&self, hash: &[u8; 32]) -> Result<Option<bitcoin::Block>, BtcError>;
    /// The block header, its height and whether it is on the best chain; None when unknown.
    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError>;
    /// The transaction (needs txindex) and the best-chain block it is in; None when unknown.
    ///
    /// A transaction that is neither on the best chain nor in the mempool (e.g. one that appears only in a reorged-out
    /// block) counts as unknown.
    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError>;
    /// The unspent output (gettxout); when `include_mempool` is true, outputs spent by mempool transactions count as
    /// spent and new outputs in the mempool are visible too (0 confirmations). None when spent or nonexistent.
    fn utxo(
        &self,
        outpoint: &bitcoin::OutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError>;
    /// All confirmed UTXOs whose locking script is `script` (scantxoutset, which looks only at the UTXO set, not at
    /// the mempool, or a wallet watching `script` that answers the same, D-109), in ascending (height, outpoint)
    /// order.
    fn scan_utxos(&self, script: &bitcoin::Script) -> Result<Vec<ScannedUtxo>, BtcError>;
    /// testmempoolaccept: `txs` are checked in order (parents first) as one package, and none is added to the mempool
    /// (the settlement's commit and reveal transactions, D-081). Ok(Ok(())) all accepted, Ok(Err(reason)) one of them
    /// was rejected (the first rejection reason).
    fn test_accept(&self, txs: &[&bitcoin::Transaction]) -> Result<Result<(), String>, BtcError>;
    /// sendrawtransaction: Ok(Ok(())) already in the mempool or already confirmed; Ok(Err(reason)) rejected.
    fn broadcast(&self, tx: &bitcoin::Transaction) -> Result<Result<(), String>, BtcError>;
    /// Broadcasts `txs` in order (parents first, e.g. the settlement's commit and reveal transactions): stops when one
    /// is rejected and returns its reason.
    fn broadcast_all(&self, txs: &[&bitcoin::Transaction]) -> Result<Result<(), String>, BtcError> {
        for tx in txs {
            if let Err(reason) = self.broadcast(tx)? {
                return Ok(Err(reason));
            }
        }
        Ok(Ok(()))
    }
    /// Whether the transaction is in the mempool.
    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError>;
    /// The IDs of all transactions in the mempool (`getrawmempool`, internal byte order).
    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError>;
    /// The height txindex has synced to (`getindexinfo`); `None` when txindex is not enabled. txindex updates
    /// asynchronously after a new block connects; until it catches up, "not found in txindex" cannot be taken as "the
    /// transaction does not exist".
    fn txindex_height(&self) -> Result<Option<u32>, BtcError>;
    /// bitcoind's fee rate estimate for confirming within `target` BTC blocks (`estimatesmartfee`, conservative mode),
    /// in sat/vB rounded up; `None` when bitcoind has none (too little data, e.g. on a fresh regtest). Only the node's
    /// local fee policy reads it (D-102), never consensus code. Without an implementation there is no estimate.
    fn estimate_feerate(&self, target: u16) -> Result<Option<u64>, BtcError> {
        let _ = target;
        Ok(None)
    }
    /// What bitcoind's mempool holds now (`getmempoolinfo`); `None` while bitcoind is still loading the mempool it
    /// kept across a restart, which may hold less than it will. Only the node's local fee policy reads it (D-117),
    /// never consensus code. Without an implementation it is not known.
    fn mempool_info(&self) -> Result<Option<MempoolInfo>, BtcError> {
        Ok(None)
    }
}

impl<T: BtcClient + ?Sized> BtcClient for Arc<T> {
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
        (**self).tip()
    }

    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
        (**self).block_hash(height)
    }

    fn block(&self, hash: &[u8; 32]) -> Result<Option<Block>, BtcError> {
        (**self).block(hash)
    }

    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
        (**self).header(hash)
    }

    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
        (**self).transaction(txid)
    }

    fn utxo(
        &self,
        outpoint: &OutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError> {
        (**self).utxo(outpoint, include_mempool)
    }

    fn scan_utxos(&self, script: &Script) -> Result<Vec<ScannedUtxo>, BtcError> {
        (**self).scan_utxos(script)
    }

    fn test_accept(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
        (**self).test_accept(txs)
    }

    fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
        (**self).broadcast(tx)
    }

    fn broadcast_all(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
        (**self).broadcast_all(txs)
    }

    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
        (**self).in_mempool(txid)
    }

    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
        (**self).mempool_txids()
    }

    fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
        (**self).txindex_height()
    }

    fn estimate_feerate(&self, target: u16) -> Result<Option<u64>, BtcError> {
        (**self).estimate_feerate(target)
    }

    fn mempool_info(&self) -> Result<Option<MempoolInfo>, BtcError> {
        (**self).mempool_info()
    }
}

/// Retry count and interval when `scantxoutset` hits "another scan is in progress".
const SCAN_BUSY_RETRIES: u32 = 50;
const SCAN_BUSY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
/// The number of locking scripts whose last scan is kept; all are dropped when it is reached.
const SCAN_CACHE_LIMIT: usize = 16;

// Bitcoin Core's JSON-RPC error codes (`src/rpc/protocol.h`).
/// The object does not exist (block, transaction, mempool entry).
const RPC_INVALID_ADDRESS_OR_KEY: i32 = -5;
/// Invalid parameter; `getblockhash` returns it when the height is out of range.
const RPC_INVALID_PARAMETER: i32 = -8;
/// The transaction cannot be decoded.
const RPC_DESERIALIZATION_ERROR: i32 = -22;
/// The transaction was rejected: inputs missing or spent, fee limit exceeded, etc.
const RPC_VERIFY_ERROR: i32 = -25;
/// The transaction was rejected by the mempool.
const RPC_VERIFY_REJECTED: i32 = -26;
/// The transaction is already on the chain (and still has unspent outputs).
const RPC_VERIFY_ALREADY_IN_CHAIN: i32 = -27;

/// The entry limit of the output cache; it is cleared entirely when reached.
const TXOUT_CACHE_LIMIT: usize = 100_000;

/// A [`BtcClient`] based on the `corepc-client` synchronous client (Bitcoin Core v28 interface).
///
/// bitcoind must have `-txindex` enabled (to fetch raw transactions). Construction does not connect to the node;
/// connection errors are returned as [`BtcError::Rpc`] on the first call. Outputs are cached by outpoint (the
/// transaction ID commits to all outputs, so their content never changes), which avoids fetching the same transaction
/// again when the same address is scanned repeatedly. A UTXO set scan is kept with the BTC tip it read: the set changes
/// only with the tip, so every caller (the producer and the rescuer of a node) shares one scan of a locking script per
/// tip instead of reading the whole set again each round (D-109). With a wallet ([`RpcClient::with_wallet`]) the
/// outputs of the script it watches are listed from the wallet instead of a scan.
pub struct RpcClient {
    url: String,
    auth: Auth,
    client: Client,
    txouts: Mutex<BTreeMap<OutPoint, TxOut>>,
    /// The last scan of each locking script.
    scans: Mutex<BTreeMap<ScriptBuf, Scan>>,
    wallet: Option<Watch>,
}

/// A watch-only wallet of the same bitcoind that lists the outputs of one locking script in place of a UTXO set scan
/// (D-109).
struct Watch {
    name: String,
    /// The same bitcoind at the wallet's endpoint, `<url>/wallet/<name>`.
    client: Client,
    /// The locking script the wallet watches.
    script: ScriptBuf,
    /// The outputs of `script` the wallet has listed and the UTXO set still held when last asked
    /// ([`RpcClient::list_watched`]).
    seen: Mutex<BTreeSet<OutPoint>>,
}

/// A UTXO set scan of one locking script.
struct Scan {
    /// The hash of the BTC tip the scan read (`scantxoutset`'s `bestblock`).
    tip: [u8; 32],
    found: Vec<ScannedUtxo>,
}

impl fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // do not print the inner client: its Debug contains the credentials.
        f.debug_struct("RpcClient")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// A failed RPC call.
enum CallError {
    /// A JSON-RPC error returned by bitcoind.
    Rpc { code: i32, message: String },
    /// A connection, HTTP or response format error.
    Other(String),
}

impl CallError {
    fn into_btc(self, method: &str) -> BtcError {
        match self {
            CallError::Rpc { code, message } => {
                BtcError::Rpc(format!("{method}: {message} (error code {code})"))
            }
            CallError::Other(e) => BtcError::Rpc(format!("{method}: {e}")),
        }
    }
}

impl RpcClient {
    /// Connects to `url` (e.g. `http://127.0.0.1:18443`) with username / password authentication.
    pub fn new(url: &str, user: &str, pass: &str) -> Result<Self, BtcError> {
        let client = Client::new_with_auth(url, Auth::UserPass(user.to_owned(), pass.to_owned()))
            .map_err(|e| BtcError::Rpc(format!("connecting to {url}: {e}")))?;
        Ok(Self {
            url: url.to_owned(),
            auth: Auth::UserPass(user.to_owned(), pass.to_owned()),
            client,
            txouts: Mutex::new(BTreeMap::new()),
            scans: Mutex::new(BTreeMap::new()),
            wallet: None,
        })
    }

    /// Lists the confirmed outputs of `script` from the wallet `wallet` of the same bitcoind instead of scanning the
    /// whole UTXO set for them (D-109): a watch-only wallet holding a descriptor of `script` (e.g.
    /// `raw(<locking script hex>)`), loaded in bitcoind, which keeps it current block by block. A scan of the UTXO
    /// set takes as long as the set is large, minutes on mainnet's, while the wallet answers at once.
    /// [`BtcClient::scan_utxos`] of any other script still scans. Like [`RpcClient::new`], this does not connect.
    pub fn with_wallet(mut self, wallet: &str, script: ScriptBuf) -> Result<Self, BtcError> {
        // the name goes into the URL path
        if wallet.is_empty()
            || !wallet
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(BtcError::Rpc(format!(
                "wallet name {wallet:?}: only letters, digits, '.', '_' and '-'"
            )));
        }
        let url = format!("{}/wallet/{wallet}", self.url.trim_end_matches('/'));
        let client = Client::new_with_auth(&url, self.auth.clone())
            .map_err(|e| BtcError::Rpc(format!("connecting to {url}: {e}")))?;
        self.wallet = Some(Watch {
            name: wallet.to_owned(),
            client,
            script,
            seen: Mutex::new(BTreeSet::new()),
        });
        Ok(self)
    }

    /// Whether the wallet of [`RpcClient::with_wallet`] watches `address`, which must be the address of the script
    /// it was given (`getaddressinfo`'s `ismine`); `None` without a wallet.
    pub fn wallet_watches(&self, address: &str) -> Result<Option<bool>, BtcError> {
        const METHOD: &str = "getaddressinfo";
        let Some(watch) = &self.wallet else {
            return Ok(None);
        };
        let info = call_on(&watch.client, METHOD, &[json!(address)])
            .map_err(|e| e.into_btc(&format!("{METHOD} (wallet {})", watch.name)))?;
        if str_field(&info, "scriptPubKey", METHOD)? != hex::encode(watch.script.as_bytes()) {
            return Err(BtcError::Decode(format!(
                "{METHOD}: {address} is not the address of the script wallet {} watches",
                watch.name
            )));
        }
        Ok(Some(bool_field(&info, "ismine", METHOD)?))
    }

    fn call(&self, method: &str, args: &[Value]) -> Result<Value, CallError> {
        call_on(&self.client, method, args)
    }

    /// Calls the RPC; any failure is a local fault.
    fn call_ok(&self, method: &str, args: &[Value]) -> Result<Value, BtcError> {
        self.call(method, args).map_err(|e| e.into_btc(method))
    }

    /// Calls the RPC; `None` when bitcoind returns error code `absent` (the object does not exist).
    fn call_opt(
        &self,
        method: &str,
        args: &[Value],
        absent: i32,
    ) -> Result<Option<Value>, BtcError> {
        match self.call(method, args) {
            Ok(value) => Ok(Some(value)),
            Err(CallError::Rpc { code, .. }) if code == absent => Ok(None),
            Err(e) => Err(e.into_btc(method)),
        }
    }

    /// The raw transaction (mempool or txindex, including transactions only in disconnected blocks); `None` when it
    /// does not exist.
    fn raw_transaction(&self, txid: &[u8; 32]) -> Result<Option<Transaction>, BtcError> {
        const METHOD: &str = "getrawtransaction";
        let args = [json!(to_display_hex(txid)), json!(false)];
        let Some(value) = self.call_opt(METHOD, &args, RPC_INVALID_ADDRESS_OR_KEY)? else {
            return Ok(None);
        };
        let tx: Transaction = decode_hex(as_str(&value, METHOD)?, METHOD)?;
        check_txid(&tx, txid, METHOD)?;
        Ok(Some(tx))
    }

    fn cache(&self) -> MutexGuard<'_, BTreeMap<OutPoint, TxOut>> {
        self.txouts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The output at `outpoint`: checks the cache first, then fetches the raw transaction (and caches all of its
    /// outputs). Called only for outputs known to exist: a transaction not being found is a local fault (bitcoind
    /// without txindex, or reorged out during the query).
    fn txout(&self, outpoint: &OutPoint) -> Result<TxOut, BtcError> {
        if let Some(out) = self.cache().get(outpoint) {
            return Ok(out.clone());
        }
        let txid = outpoint.txid.to_byte_array();
        let tx = self.raw_transaction(&txid)?.ok_or_else(|| {
            BtcError::Rpc(format!(
                "getrawtransaction: transaction {} not found (bitcoind needs -txindex)",
                outpoint.txid
            ))
        })?;
        let out = usize::try_from(outpoint.vout)
            .ok()
            .and_then(|vout| tx.output.get(vout))
            .cloned()
            .ok_or_else(|| {
                BtcError::Decode(format!(
                    "transaction {} has no output {}",
                    outpoint.txid, outpoint.vout
                ))
            })?;
        let mut cache = self.cache();
        if cache.len() >= TXOUT_CACHE_LIMIT {
            cache.clear();
        }
        for (vout, output) in (0u32..).zip(&tx.output) {
            cache.insert(
                OutPoint {
                    txid: outpoint.txid,
                    vout,
                },
                output.clone(),
            );
        }
        Ok(out)
    }

    /// One `scantxoutset` of `script`: its outputs, and the hash of the BTC tip the scan read (`bestblock`; `None` when
    /// bitcoind does not give it).
    fn scan_now(&self, script: &Script) -> Result<(Option<[u8; 32]>, Vec<ScannedUtxo>), BtcError> {
        const METHOD: &str = "scantxoutset";
        let desc = format!("raw({})", hex::encode(script.as_bytes()));
        let args = [json!("start"), json!([{ "desc": desc }])];
        // bitcoind runs only one scan at a time: wait a little and retry while other nodes sharing the bitcoind are
        // scanning
        let mut attempts = 0u32;
        let result = loop {
            match self.call(METHOD, &args) {
                Ok(result) => break result,
                Err(CallError::Rpc {
                    code: RPC_INVALID_PARAMETER,
                    message,
                }) if message.contains("Scan already in progress")
                    && attempts < SCAN_BUSY_RETRIES =>
                {
                    attempts = attempts.saturating_add(1);
                    std::thread::sleep(SCAN_BUSY_DELAY);
                }
                Err(e) => return Err(e.into_btc(METHOD)),
            }
        };
        if result.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(BtcError::Rpc(format!("{METHOD}: scan did not complete")));
        }
        let unspents = result
            .get("unspents")
            .and_then(Value::as_array)
            .ok_or_else(|| BtcError::Decode(format!("{METHOD}: missing unspents")))?;
        let mut found = Vec::with_capacity(unspents.len());
        for unspent in unspents {
            let outpoint = OutPoint {
                txid: Txid::from_byte_array(hash_field(unspent, "txid", METHOD)?),
                vout: u32_field(unspent, "vout", METHOD)?,
            };
            let txout = self.txout(&outpoint)?;
            if txout.script_pubkey.as_script() != script {
                return Err(BtcError::Decode(format!(
                    "{METHOD}: the locking script of output {outpoint} does not match the request"
                )));
            }
            found.push(ScannedUtxo {
                outpoint,
                txout,
                height: u32_field(unspent, "height", METHOD)?,
                coinbase: bool_field(unspent, "coinbase", METHOD)?,
            });
        }
        found.sort_by_key(|u| (u.height, u.outpoint));
        let at = result
            .get("bestblock")
            .map(|value| hash_value(value, METHOD))
            .transpose()?;
        Ok((at, found))
    }

    /// The confirmed outputs of the watched script, as a scan of the UTXO set would find them (D-109): the wallet says
    /// which outputs to look at, the UTXO set whether each is still unspent, its height and whether it is a coinbase
    /// output. Every output the wallet has listed is asked again until the UTXO set no longer holds it: bitcoind's
    /// wallet counts an output as spent by a transaction that left the mempool unconfirmed until that transaction is
    /// abandoned, while the output is still there to spend.
    fn list_watched(&self, watch: &Watch) -> Result<Vec<ScannedUtxo>, BtcError> {
        const METHOD: &str = "listunspent";
        const COIN: &str = "gettxout";
        // confirmed outputs, immature coinbase outputs too: the callers check maturity, as after a scan
        let args = [
            json!(1),
            json!(9_999_999),
            json!([]),
            json!(true),
            json!({ "include_immature_coinbase": true }),
        ];
        let listed = call_on(&watch.client, METHOD, &args)
            .map_err(|e| e.into_btc(&format!("{METHOD} (wallet {})", watch.name)))?;
        let entries = listed
            .as_array()
            .ok_or_else(|| BtcError::Decode(format!("{METHOD}: the result is not an array")))?;
        let wanted = hex::encode(watch.script.as_bytes());
        let mut seen = watch.seen.lock().unwrap_or_else(PoisonError::into_inner);
        for entry in entries {
            // the wallet may watch other scripts too
            if str_field(entry, "scriptPubKey", METHOD)? == wanted {
                seen.insert(OutPoint {
                    txid: Txid::from_byte_array(hash_field(entry, "txid", METHOD)?),
                    vout: u32_field(entry, "vout", METHOD)?,
                });
            }
        }
        let mut found = Vec::with_capacity(seen.len());
        let mut spent = Vec::new();
        // the height of each best block the answers name
        let mut tips = BTreeMap::new();
        for outpoint in seen.iter() {
            let args = [
                json!(outpoint.txid.to_string()),
                json!(outpoint.vout),
                json!(false),
            ];
            let coin = self.call_ok(COIN, &args)?;
            if coin.is_null() {
                spent.push(*outpoint);
                continue;
            }
            let best = hash_field(&coin, "bestblock", COIN)?;
            let tip = match tips.get(&best) {
                Some(height) => *height,
                None => {
                    let header = self.call_ok(
                        "getblockheader",
                        &[json!(to_display_hex(&best)), json!(true)],
                    )?;
                    let height = u32_field(&header, "height", "getblockheader")?;
                    tips.insert(best, height);
                    height
                }
            };
            let confirmations = u32_field(&coin, "confirmations", COIN)?;
            let height = tip
                .checked_add(1)
                .and_then(|next| next.checked_sub(confirmations))
                .ok_or_else(|| {
                    BtcError::Decode(format!(
                        "{COIN}: {confirmations} confirmations at height {tip}"
                    ))
                })?;
            let txout = self.txout(outpoint)?;
            if txout.script_pubkey != watch.script {
                return Err(BtcError::Decode(format!(
                    "{METHOD}: the locking script of output {outpoint} is not the one wallet {} watches",
                    watch.name
                )));
            }
            found.push(ScannedUtxo {
                outpoint: *outpoint,
                txout,
                height,
                coinbase: bool_field(&coin, "coinbase", COIN)?,
            });
        }
        for outpoint in spent {
            seen.remove(&outpoint);
        }
        found.sort_by_key(|u| (u.height, u.outpoint));
        Ok(found)
    }
}

/// One JSON-RPC call through `client`.
fn call_on(client: &Client, method: &str, args: &[Value]) -> Result<Value, CallError> {
    client
        .call::<Value>(method, args)
        .map_err(|e| match rpc_error(&e) {
            Some((code, message)) => CallError::Rpc { code, message },
            None => CallError::Other(e.to_string()),
        })
}

impl BtcClient for RpcClient {
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
        // `blocks` and `bestblockhash` are read under the same lock inside bitcoind and belong to the same block.
        const METHOD: &str = "getblockchaininfo";
        let info = self.call_ok(METHOD, &[])?;
        Ok((
            u32_field(&info, "blocks", METHOD)?,
            hash_field(&info, "bestblockhash", METHOD)?,
        ))
    }

    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
        const METHOD: &str = "getblockhash";
        // bitcoind's height is an int: an out-of-range value is treated as a parameter format error, not as "height
        // out of range".
        if i32::try_from(height).is_err() {
            return Ok(None);
        }
        match self.call_opt(METHOD, &[json!(height)], RPC_INVALID_PARAMETER)? {
            Some(value) => Ok(Some(hash_value(&value, METHOD)?)),
            None => Ok(None),
        }
    }

    fn block(&self, hash: &[u8; 32]) -> Result<Option<Block>, BtcError> {
        const METHOD: &str = "getblock";
        let args = [json!(to_display_hex(hash)), json!(0)];
        let Some(value) = self.call_opt(METHOD, &args, RPC_INVALID_ADDRESS_OR_KEY)? else {
            return Ok(None);
        };
        let block: Block = decode_hex(as_str(&value, METHOD)?, METHOD)?;
        if block.block_hash().to_byte_array() != *hash {
            return Err(BtcError::Decode(format!(
                "{METHOD}: the returned block hash does not match the request"
            )));
        }
        Ok(Some(block))
    }

    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
        const METHOD: &str = "getblockheader";
        let hex = to_display_hex(hash);
        let Some(verbose) = self.call_opt(
            METHOD,
            &[json!(hex), json!(true)],
            RPC_INVALID_ADDRESS_OR_KEY,
        )?
        else {
            return Ok(None);
        };
        let height = u32_field(&verbose, "height", METHOD)?;
        // a block not on the best chain has confirmations -1.
        let in_best_chain = i64_field(&verbose, "confirmations", METHOD)? > 0;
        // entries in the block index are never deleted, so the second query cannot become "does not exist"; the
        // header content is immutable.
        let Some(raw) = self.call_opt(
            METHOD,
            &[json!(hex), json!(false)],
            RPC_INVALID_ADDRESS_OR_KEY,
        )?
        else {
            return Ok(None);
        };
        let header: bitcoin::block::Header = decode_hex(as_str(&raw, METHOD)?, METHOD)?;
        if header.block_hash().to_byte_array() != *hash {
            return Err(BtcError::Decode(format!(
                "{METHOD}: the returned header hash does not match the request"
            )));
        }
        Ok(Some(HeaderInfo {
            height,
            in_best_chain,
            header,
        }))
    }

    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
        const METHOD: &str = "getrawtransaction";
        let args = [json!(to_display_hex(txid)), json!(true)];
        let Some(verbose) = self.call_opt(METHOD, &args, RPC_INVALID_ADDRESS_OR_KEY)? else {
            return Ok(None);
        };
        let tx: Transaction = decode_hex(str_field(&verbose, "hex", METHOD)?, METHOD)?;
        check_txid(&tx, txid, METHOD)?;
        // bitcoind checks the mempool first (no blockhash), then txindex. txindex does not delete entries from
        // disconnected blocks: such a transaction is returned with the old block's hash and confirmations 0; it is
        // neither on the best chain nor in the mempool.
        match verbose.get("blockhash") {
            None | Some(Value::Null) => Ok(Some(TxInfo { tx, block: None })),
            Some(block) => {
                let block = hash_value(block, METHOD)?;
                let confirmations = verbose
                    .get("confirmations")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                Ok((confirmations > 0).then_some(TxInfo {
                    tx,
                    block: Some(block),
                }))
            }
        }
    }

    fn utxo(
        &self,
        outpoint: &OutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError> {
        const METHOD: &str = "gettxout";
        let args = [
            json!(outpoint.txid.to_string()),
            json!(outpoint.vout),
            json!(include_mempool),
        ];
        let value = self.call_ok(METHOD, &args)?;
        if value.is_null() {
            return Ok(None);
        }
        let confirmations = u32_field(&value, "confirmations", METHOD)?;
        let coinbase = bool_field(&value, "coinbase", METHOD)?;
        Ok(Some(UtxoInfo {
            txout: self.txout(outpoint)?,
            confirmations,
            coinbase,
        }))
    }

    fn scan_utxos(&self, script: &Script) -> Result<Vec<ScannedUtxo>, BtcError> {
        // scantxoutset reads the whole UTXO set, tens of seconds on signet's or mainnet's, and the set changes only with
        // the tip: a scan of the same script at the same tip is answered from the last one. The lock is held through a
        // scan, so a second caller waits for it and takes its result instead of asking bitcoind, which runs one scan at
        // a time, for another
        let mut scans = self.scans.lock().unwrap_or_else(PoisonError::into_inner);
        let tip = hash_value(&self.call_ok("getbestblockhash", &[])?, "getbestblockhash")?;
        if let Some(last) = scans.get(script) {
            if last.tip == tip {
                return Ok(last.found.clone());
            }
        }
        let (at, found) = match &self.wallet {
            // the wallet's answer is at least as new as `tip`: listunspent waits until the wallet has caught up with
            // the chain, and gettxout reads the UTXO set at the time of the call
            Some(watch) if watch.script.as_script() == script => {
                (Some(tip), self.list_watched(watch)?)
            }
            _ => self.scan_now(script)?,
        };
        // the tip the scan read, which may be newer than `tip`; a bitcoind that does not say is asked again next time
        if let Some(at) = at {
            if scans.len() >= SCAN_CACHE_LIMIT {
                scans.clear();
            }
            scans.insert(
                script.to_owned(),
                Scan {
                    tip: at,
                    found: found.clone(),
                },
            );
        }
        Ok(found)
    }

    fn test_accept(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
        const METHOD: &str = "testmempoolaccept";
        let raw: Vec<String> = txs.iter().map(|tx| serialize_hex(*tx)).collect();
        let result = match self.call(METHOD, &[json!(raw)]) {
            Ok(result) => result,
            Err(CallError::Rpc {
                code: RPC_DESERIALIZATION_ERROR,
                message,
            }) => return Ok(Err(message)),
            Err(e) => return Err(e.into_btc(METHOD)),
        };
        let entries = result
            .as_array()
            .filter(|entries| entries.len() == txs.len())
            .ok_or_else(|| {
                BtcError::Decode(format!(
                    "{METHOD}: the result does not match the number of transactions"
                ))
            })?;
        for entry in entries {
            if entry.get("allowed").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            // rejected: reject-reason (from Bitcoin Core 29 also reject-details, "reason, details"); when another
            // transaction in the package failed and this one was not fully checked there is no allowed, only
            // package-error
            let reason = entry.get("reject-reason").and_then(Value::as_str);
            let details = entry.get("reject-details").and_then(Value::as_str);
            let package = entry.get("package-error").and_then(Value::as_str);
            return Ok(Err(match (reason, details) {
                (Some(reason), Some(details)) if details.starts_with(reason) => details.to_owned(),
                (Some(reason), Some(details)) => format!("{reason}: {details}"),
                (Some(reason), None) => reason.to_owned(),
                (None, _) => package.unwrap_or("unknown reason").to_owned(),
            }));
        }
        Ok(Ok(()))
    }

    fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
        const METHOD: &str = "sendrawtransaction";
        match self.call(METHOD, &[json!(serialize_hex(tx))]) {
            // success: entered the mempool, or the same transaction was already in the mempool.
            Ok(_) => Ok(Ok(())),
            Err(CallError::Rpc {
                code: RPC_VERIFY_ALREADY_IN_CHAIN,
                ..
            }) => Ok(Ok(())),
            Err(CallError::Rpc {
                code: RPC_DESERIALIZATION_ERROR,
                message,
            }) => Ok(Err(message)),
            Err(CallError::Rpc { code, message })
                if code == RPC_VERIFY_ERROR || code == RPC_VERIFY_REJECTED =>
            {
                // a confirmed transaction whose outputs have all been spent is rejected with "inputs missing or spent"
                // (-25), and so is a transaction mined during the query: it counts as rejected only once it is
                // confirmed to be neither on the chain nor in the mempool.
                match self.transaction(&tx.compute_txid().to_byte_array())? {
                    Some(_) => Ok(Ok(())),
                    None => Ok(Err(message)),
                }
            }
            Err(e) => Err(e.into_btc(METHOD)),
        }
    }

    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
        let args = [json!(to_display_hex(txid))];
        Ok(self
            .call_opt("getmempoolentry", &args, RPC_INVALID_ADDRESS_OR_KEY)?
            .is_some())
    }

    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
        const METHOD: &str = "getrawmempool";
        let list = self.call_ok(METHOD, &[])?;
        let items = list
            .as_array()
            .ok_or_else(|| BtcError::Decode(format!("{METHOD}: the result is not an array")))?;
        items
            .iter()
            .map(|item| {
                item.as_str().and_then(from_display_hex).ok_or_else(|| {
                    BtcError::Decode(format!("{METHOD}: malformed transaction ID: {item}"))
                })
            })
            .collect()
    }

    fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
        const METHOD: &str = "getindexinfo";
        let info = self.call_ok(METHOD, &[json!("txindex")])?;
        match info.get("txindex") {
            Some(index) => Ok(Some(u32_field(index, "best_block_height", METHOD)?)),
            None => Ok(None),
        }
    }

    fn estimate_feerate(&self, target: u16) -> Result<Option<u64>, BtcError> {
        const METHOD: &str = "estimatesmartfee";
        let result = self.call_ok(METHOD, &[json!(target), json!("conservative")])?;
        // Without enough data there is no `feerate`, only `errors`
        match result.get("feerate") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(rate)) => sat_per_vb(&rate.to_string())
                .map(Some)
                .ok_or_else(|| BtcError::Decode(format!("{METHOD}: malformed fee rate {rate}"))),
            Some(other) => Err(BtcError::Decode(format!(
                "{METHOD}: the fee rate is not a number: {other}"
            ))),
        }
    }

    fn mempool_info(&self) -> Result<Option<MempoolInfo>, BtcError> {
        const METHOD: &str = "getmempoolinfo";
        let info = self.call_ok(METHOD, &[])?;
        // Still loading mempool.dat after a restart
        if !bool_field(&info, "loaded", METHOD)? {
            return Ok(None);
        }
        let min_feerate = match field(&info, "mempoolminfee", METHOD)? {
            Value::Number(rate) => sat_per_vb(&rate.to_string())
                .ok_or_else(|| BtcError::Decode(format!("{METHOD}: malformed fee rate {rate}")))?,
            other => {
                return Err(BtcError::Decode(format!(
                    "{METHOD}: the fee rate is not a number: {other}"
                )))
            }
        };
        Ok(Some(MempoolInfo {
            vsize: u64_field(&info, "bytes", METHOD)?,
            min_feerate,
        }))
    }
}

/// A fee rate in BTC/kvB as bitcoind writes it (a decimal number, possibly with an exponent, e.g. `0.00012345` or
/// `1e-05`) converted to sat/vB, rounded up, without floating-point arithmetic; `None` when it is not a non-negative
/// decimal number or does not fit.
fn sat_per_vb(text: &str) -> Option<u64> {
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(at) => (
            text.get(..at)?,
            text.get(at.checked_add(1)?..)?.parse::<i32>().ok()?,
        ),
        None => (text, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let digits = [whole, fraction].concat();
    if digits.len() > 30 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    // value × 10^(exponent − fraction digits) BTC/kvB = value × 10^(exponent − fraction digits + 5) sat/vB
    let scale = i64::from(exponent)
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(5)?;
    let sats = if scale >= 0 {
        value.checked_mul(10u128.checked_pow(u32::try_from(scale).ok()?)?)?
    } else {
        match 10u128.checked_pow(u32::try_from(scale.checked_neg()?).ok()?) {
            Some(divisor) => value.div_ceil(divisor),
            // smaller than any representable divisor: a positive value still rounds up to 1
            None => u128::from(value > 0),
        }
    };
    u64::try_from(sats).ok()
}

/// The JSON-RPC error returned by bitcoind (error code, message); `None` for connection, HTTP and response format
/// errors.
///
/// `corepc-client` does not re-export `jsonrpc`, so `jsonrpc::Error::Rpc` cannot be matched by type; it is the derived
/// Debug implementation, whose output is `Rpc(RpcError { code: -8, message: "…", data: None })`, and the version is
/// locked by Cargo.lock (the unit tests of this module check the format).
fn rpc_error(err: &CorepcError) -> Option<(i32, String)> {
    let CorepcError::JsonRpc(inner) = err else {
        return None;
    };
    let text = format!("{inner:?}");
    let rest = text.strip_prefix("Rpc(RpcError { code: ")?;
    let (code, rest) = rest.split_once(", message: ")?;
    Some((code.parse().ok()?, parse_debug_str(rest)?))
}

/// Parses the Rust Debug string literal at the start of `rest` (`"…"`, with the escapes of `str::escape_debug`) and
/// returns its content.
fn parse_debug_str(rest: &str) -> Option<String> {
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '0' => out.push('\0'),
                'u' => {
                    if chars.next()? != '{' {
                        return None;
                    }
                    let mut code: u32 = 0;
                    loop {
                        let c = chars.next()?;
                        if c == '}' {
                            break;
                        }
                        code = code.checked_mul(16)?.checked_add(c.to_digit(16)?)?;
                    }
                    out.push(char::from_u32(code)?);
                }
                // `\\`, `\"`, `\'`
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

/// RPC hex (display byte order) → internal byte order.
fn from_display_hex(hex_str: &str) -> Option<[u8; 32]> {
    let mut bytes: [u8; 32] = hex::decode(hex_str).ok()?.try_into().ok()?;
    bytes.reverse();
    Some(bytes)
}

/// Internal byte order → RPC hex (display byte order).
fn to_display_hex(hash: &[u8; 32]) -> String {
    let mut bytes = *hash;
    bytes.reverse();
    hex::encode(bytes)
}

fn decode_hex<T: Decodable>(hex_str: &str, method: &str) -> Result<T, BtcError> {
    deserialize_hex(hex_str).map_err(|e| BtcError::Decode(format!("{method}: {e}")))
}

fn check_txid(tx: &Transaction, txid: &[u8; 32], method: &str) -> Result<(), BtcError> {
    if tx.compute_txid().to_byte_array() == *txid {
        Ok(())
    } else {
        Err(BtcError::Decode(format!(
            "{method}: the returned transaction ID does not match the request"
        )))
    }
}

fn field<'a>(value: &'a Value, key: &str, method: &str) -> Result<&'a Value, BtcError> {
    value
        .get(key)
        .ok_or_else(|| BtcError::Decode(format!("{method}: missing field {key}")))
}

fn as_str<'a>(value: &'a Value, method: &str) -> Result<&'a str, BtcError> {
    value
        .as_str()
        .ok_or_else(|| BtcError::Decode(format!("{method}: the result is not a string")))
}

fn str_field<'a>(value: &'a Value, key: &str, method: &str) -> Result<&'a str, BtcError> {
    field(value, key, method)?
        .as_str()
        .ok_or_else(|| BtcError::Decode(format!("{method}: field {key} is not a string")))
}

fn u32_field(value: &Value, key: &str, method: &str) -> Result<u32, BtcError> {
    field(value, key, method)?
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| BtcError::Decode(format!("{method}: field {key} is not a u32")))
}

fn u64_field(value: &Value, key: &str, method: &str) -> Result<u64, BtcError> {
    field(value, key, method)?
        .as_u64()
        .ok_or_else(|| BtcError::Decode(format!("{method}: field {key} is not a u64")))
}

fn i64_field(value: &Value, key: &str, method: &str) -> Result<i64, BtcError> {
    field(value, key, method)?
        .as_i64()
        .ok_or_else(|| BtcError::Decode(format!("{method}: field {key} is not an integer")))
}

fn bool_field(value: &Value, key: &str, method: &str) -> Result<bool, BtcError> {
    field(value, key, method)?
        .as_bool()
        .ok_or_else(|| BtcError::Decode(format!("{method}: field {key} is not a boolean")))
}

fn hash_value(value: &Value, method: &str) -> Result<[u8; 32], BtcError> {
    value
        .as_str()
        .and_then(from_display_hex)
        .ok_or_else(|| BtcError::Decode(format!("{method}: not a 32-byte hex hash")))
}

fn hash_field(value: &Value, key: &str, method: &str) -> Result<[u8; 32], BtcError> {
    hash_value(field(value, key, method)?, method)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

    use bitcoin::{absolute, transaction, Amount, ScriptBuf, Sequence, TxIn, Witness};

    use super::*;

    #[test]
    fn fee_rates_convert_to_sat_per_vb_rounding_up() {
        assert_eq!(sat_per_vb("0.00001"), Some(1));
        assert_eq!(sat_per_vb("0.00012345"), Some(13));
        assert_eq!(sat_per_vb("0.0002"), Some(20));
        assert_eq!(sat_per_vb("1e-05"), Some(1));
        assert_eq!(sat_per_vb("1.5E-4"), Some(15));
        assert_eq!(sat_per_vb("2.5e-7"), Some(1));
        assert_eq!(sat_per_vb("0.001"), Some(100));
        assert_eq!(sat_per_vb("1"), Some(100_000));
        assert_eq!(sat_per_vb("0"), Some(0));
        assert_eq!(sat_per_vb("1e-60"), Some(1));
        assert_eq!(sat_per_vb("0e-60"), Some(0));
        for bad in [
            "", ".", "-0.0001", "abc", "1e", "1e400", "0x10", "1.2.3", "1e30",
        ] {
            assert_eq!(sat_per_vb(bad), None, "{bad}");
        }
    }

    #[test]
    fn display_hex_is_reversed_internal_order() {
        let txid =
            Txid::from_str("4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b")
                .unwrap();
        let internal = txid.to_byte_array();
        assert_eq!(to_display_hex(&internal), txid.to_string());
        assert_eq!(from_display_hex(&txid.to_string()), Some(internal));
        assert_eq!(from_display_hex("00"), None);
        assert_eq!(from_display_hex("zz"), None);
    }

    #[test]
    fn debug_string_literals_are_unescaped() {
        for s in [
            "",
            "Block height out of range",
            "bad \"quoted\" \\ path\nline\t\r\0 end",
            "non-ASCII — é ₿ 😀 \u{7f} \u{200b}",
        ] {
            let text = format!("{s:?}, data: None }})");
            assert_eq!(parse_debug_str(&text).as_deref(), Some(s), "{text}");
        }
        assert_eq!(parse_debug_str("no quote"), None);
        assert_eq!(parse_debug_str("\"unterminated"), None);
        assert_eq!(parse_debug_str("\"\\u{zz}\""), None);
    }

    /// A minimal HTTP JSON-RPC server: `respond(method, params)` gives the result or (error code, message). Returns
    /// its URL.
    fn fake_bitcoind<F>(respond: F) -> String
    where
        F: Fn(&str, &Value) -> Result<Value, (i32, String)> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_lowercase();
                // a call to a wallet's endpoint reaches `respond` as "wallet/<name>:<method>"
                let path = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .trim_matches('/')
                    .to_owned();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if stream.read_exact(&mut body).is_err() {
                    continue;
                }
                let request: Value = serde_json::from_slice(&body).unwrap();
                let method = request["method"].as_str().unwrap_or_default();
                let method = if path.is_empty() {
                    method.to_owned()
                } else {
                    format!("{path}:{method}")
                };
                let response = match respond(&method, &request["params"]) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
                    Err((code, message)) => json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "error": {"code": code, "message": message},
                    }),
                }
                .to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
            }
        });
        url
    }

    /// The lock time tells the fake node what sendrawtransaction should return.
    fn tx_with_lock_time(lock_time: u32) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::from_consensus(lock_time),
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([7; 32]),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    const ODD_MESSAGE: &str = "bad \"quoted\" \\ reason\né";

    #[test]
    fn json_rpc_error_codes_are_mapped() {
        let url = fake_bitcoind(|method, params| {
            match method {
            "getblockhash" => Err((-8, "Block height out of range".into())),
            "getblock" | "getblockheader" | "getmempoolentry" => {
                Err((-5, "Block not found".into()))
            }
            "getrawtransaction" => Err((
                -5,
                "No such mempool or blockchain transaction. Use gettransaction for wallet transactions.".into(),
            )),
            "gettxout" => Ok(Value::Null),
            "testmempoolaccept" => Ok(json!([{
                "txid": "00",
                "allowed": false,
                "reject-reason": "min relay fee not met",
                "reject-details": "min relay fee not met, 0 < 110",
            }])),
            "sendrawtransaction" => {
                let tx: Transaction = deserialize_hex(params[0].as_str().unwrap()).unwrap();
                match tx.lock_time.to_consensus_u32() {
                    0 => Ok(json!(tx.compute_txid().to_string())),
                    1 => Err((-27, "Transaction outputs already in utxo set".into())),
                    2 => Err((-26, ODD_MESSAGE.into())),
                    3 => Err((-25, "bad-txns-inputs-missingorspent".into())),
                    4 => Err((-22, "TX decode failed".into())),
                    _ => Err((-1, "unexpected".into())),
                }
            }
            "getblockchaininfo" => Err((-28, "Loading block index…".into())),
            _ => Err((-32601, "Method not found".into())),
        }
        });
        let client = RpcClient::new(&url, "ksana", "ksana").unwrap();

        assert_eq!(client.block_hash(5), Ok(None));
        assert_eq!(client.block_hash(u32::MAX), Ok(None));
        assert_eq!(client.block(&[1; 32]), Ok(None));
        assert_eq!(client.header(&[1; 32]), Ok(None));
        assert_eq!(client.transaction(&[1; 32]), Ok(None));
        assert_eq!(client.in_mempool(&[1; 32]), Ok(false));
        let outpoint = OutPoint {
            txid: Txid::from_byte_array([1; 32]),
            vout: 0,
        };
        assert_eq!(client.utxo(&outpoint, true), Ok(None));
        assert_eq!(
            client.test_accept(&[&tx_with_lock_time(0)]),
            Ok(Err("min relay fee not met, 0 < 110".into()))
        );
        // the number of results does not match the number of transactions submitted
        assert!(matches!(
            client.test_accept(&[&tx_with_lock_time(0), &tx_with_lock_time(1)]),
            Err(BtcError::Decode(_))
        ));

        assert_eq!(client.broadcast(&tx_with_lock_time(0)), Ok(Ok(())));
        assert_eq!(client.broadcast(&tx_with_lock_time(1)), Ok(Ok(())));
        assert_eq!(
            client.broadcast(&tx_with_lock_time(2)),
            Ok(Err(ODD_MESSAGE.into()))
        );
        assert_eq!(
            client.broadcast(&tx_with_lock_time(3)),
            Ok(Err("bad-txns-inputs-missingorspent".into()))
        );
        assert_eq!(
            client.broadcast(&tx_with_lock_time(4)),
            Ok(Err("TX decode failed".into()))
        );
        // the other error codes, and errors other than "does not exist", are local faults.
        assert!(matches!(
            client.broadcast(&tx_with_lock_time(5)),
            Err(BtcError::Rpc(m)) if m.contains("unexpected") && m.contains("-1")
        ));
        assert!(matches!(client.tip(), Err(BtcError::Rpc(m)) if m.contains("-28")));
        assert!(matches!(
            client.scan_utxos(&ScriptBuf::new()),
            Err(BtcError::Rpc(_))
        ));
    }

    /// A bitcoind whose tip the test sets, answering every scan with the output of `tx_with_lock_time(0)` (its
    /// locking script is empty) as of that tip, after `delay`; `bestblock` is left out when `with_bestblock` is false.
    /// Returns the URL, the tip and the number of scans run.
    fn scanning_bitcoind(
        delay: std::time::Duration,
        with_bestblock: bool,
    ) -> (String, Arc<AtomicU8>, Arc<AtomicU32>) {
        let tx = tx_with_lock_time(0);
        let txid = tx.compute_txid().to_string();
        let raw = serialize_hex(&tx);
        let tip = Arc::new(AtomicU8::new(1));
        let scans = Arc::new(AtomicU32::new(0));
        let (at, count) = (Arc::clone(&tip), Arc::clone(&scans));
        let url = fake_bitcoind(move |method, _| {
            let best = to_display_hex(&[at.load(Ordering::SeqCst); 32]);
            match method {
                "getbestblockhash" => Ok(json!(best)),
                "scantxoutset" => {
                    count.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(delay);
                    let mut result = json!({
                        "success": true,
                        "txouts": 1000,
                        "height": 7,
                        "unspents": [{"txid": txid, "vout": 0, "height": 5, "coinbase": false}],
                    });
                    if with_bestblock {
                        result["bestblock"] = json!(best);
                    }
                    Ok(result)
                }
                "getrawtransaction" => Ok(json!(raw)),
                _ => Err((-32601, "Method not found".into())),
            }
        });
        (url, tip, scans)
    }

    #[test]
    fn a_scan_is_answered_again_until_the_tip_changes() {
        let (url, tip, scans) = scanning_bitcoind(std::time::Duration::ZERO, true);
        let client = RpcClient::new(&url, "ksana", "ksana").unwrap();
        let script = ScriptBuf::new();
        let found = client.scan_utxos(&script).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].outpoint,
            OutPoint {
                txid: tx_with_lock_time(0).compute_txid(),
                vout: 0
            }
        );
        assert_eq!(found[0].height, 5);
        assert_eq!(client.scan_utxos(&script).unwrap(), found);
        assert_eq!(
            scans.load(Ordering::SeqCst),
            1,
            "the same tip: no second scan"
        );
        tip.store(2, Ordering::SeqCst);
        assert_eq!(client.scan_utxos(&script).unwrap(), found);
        assert_eq!(scans.load(Ordering::SeqCst), 2, "a new tip: scanned again");
        assert_eq!(client.scan_utxos(&script).unwrap(), found);
        assert_eq!(scans.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_scan_without_its_tip_is_not_kept() {
        let (url, _, scans) = scanning_bitcoind(std::time::Duration::ZERO, false);
        let client = RpcClient::new(&url, "ksana", "ksana").unwrap();
        client.scan_utxos(&ScriptBuf::new()).unwrap();
        client.scan_utxos(&ScriptBuf::new()).unwrap();
        assert_eq!(scans.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_caller_waits_for_the_scan_in_progress_and_takes_its_result() {
        let (url, _, scans) = scanning_bitcoind(std::time::Duration::from_millis(300), true);
        let client = Arc::new(RpcClient::new(&url, "ksana", "ksana").unwrap());
        let callers: Vec<_> = (0..3)
            .map(|_| {
                let client = Arc::clone(&client);
                std::thread::spawn(move || client.scan_utxos(&ScriptBuf::new()).unwrap())
            })
            .collect();
        let results: Vec<_> = callers.into_iter().map(|c| c.join().unwrap()).collect();
        assert!(results.iter().all(|r| r.len() == 1 && *r == results[0]));
        assert_eq!(
            scans.load(Ordering::SeqCst),
            1,
            "one scan for the three callers"
        );
    }

    /// The state of a bitcoind with a watch-only wallet named `funding`: what its listunspent lists and what its UTXO
    /// set holds (outpoint → confirmations, coinbase), at a tip of height 10.
    #[derive(Default)]
    struct WalletState {
        tip: u8,
        listed: Vec<(OutPoint, String)>,
        coins: BTreeMap<OutPoint, (u32, bool)>,
        calls: Vec<String>,
    }

    fn wallet_bitcoind(txs: Vec<Transaction>) -> (String, Arc<Mutex<WalletState>>) {
        let state = Arc::new(Mutex::new(WalletState::default()));
        let shared = Arc::clone(&state);
        let url = fake_bitcoind(move |method, params| {
            let mut state = shared.lock().unwrap();
            state.calls.push(method.to_owned());
            let best = to_display_hex(&[state.tip; 32]);
            match method {
                "getbestblockhash" => Ok(json!(best)),
                "wallet/funding:listunspent" => {
                    assert_eq!(params[4], json!({"include_immature_coinbase": true}));
                    Ok(json!(state
                        .listed
                        .iter()
                        .map(|(o, script)| json!({
                            "txid": o.txid.to_string(),
                            "vout": o.vout,
                            "scriptPubKey": script,
                            "amount": 0.00001,
                            "confirmations": 1,
                        }))
                        .collect::<Vec<_>>()))
                }
                "wallet/funding:getaddressinfo" => Ok(json!({
                    "address": params[0],
                    "scriptPubKey": if params[0] == "watched" { "" } else { "51" },
                    "ismine": true,
                })),
                "gettxout" => {
                    assert_eq!(params[2], json!(false), "the UTXO set alone, as a scan");
                    let outpoint = OutPoint {
                        txid: params[0].as_str().unwrap().parse().unwrap(),
                        vout: u32::try_from(params[1].as_u64().unwrap()).unwrap(),
                    };
                    Ok(match state.coins.get(&outpoint) {
                        Some((confirmations, coinbase)) => json!({
                            "bestblock": best,
                            "confirmations": confirmations,
                            "coinbase": coinbase,
                        }),
                        None => Value::Null,
                    })
                }
                "getblockheader" => {
                    assert_eq!(params[0], json!(best));
                    Ok(json!({"height": 10}))
                }
                "getrawtransaction" => txs
                    .iter()
                    .find(|tx| json!(tx.compute_txid().to_string()) == params[0])
                    .map(|tx| json!(serialize_hex(tx)))
                    .ok_or((-5, "No such mempool or blockchain transaction".into())),
                _ => Err((-32601, "Method not found".into())),
            }
        });
        (url, state)
    }

    #[test]
    fn a_wallet_lists_the_outputs_of_its_script_as_the_utxo_set_holds_them() {
        let (a, b) = (tx_with_lock_time(0), tx_with_lock_time(1));
        let (a0, b0) = (
            OutPoint::new(a.compute_txid(), 0),
            OutPoint::new(b.compute_txid(), 0),
        );
        let (url, state) = wallet_bitcoind(vec![a, b]);
        {
            let mut state = state.lock().unwrap();
            state.tip = 1;
            // an output of another script the wallet watches is left out
            state.listed = vec![
                (a0, String::new()),
                (b0, String::new()),
                (
                    OutPoint::new(Txid::from_byte_array([9; 32]), 0),
                    "51".into(),
                ),
            ];
            state.coins = BTreeMap::from([(a0, (3, false)), (b0, (1, true))]);
        }
        let client = RpcClient::new(&url, "ksana", "ksana")
            .unwrap()
            .with_wallet("funding", ScriptBuf::new())
            .unwrap();
        let found = client.scan_utxos(&ScriptBuf::new()).unwrap();
        let summary: Vec<_> = found
            .iter()
            .map(|u| (u.outpoint, u.height, u.coinbase, u.txout.value.to_sat()))
            .collect();
        assert_eq!(summary, [(a0, 8, false, 1_000), (b0, 10, true, 1_000)]);
        // the same tip: answered again without asking the wallet
        assert_eq!(client.scan_utxos(&ScriptBuf::new()).unwrap(), found);
        assert_eq!(
            state
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|m| m.ends_with("listunspent"))
                .count(),
            1
        );

        // A new tip: the wallet counts b0 as spent by a transaction that left the mempool, but the UTXO set still holds
        // it; a0 was spent in the new block
        {
            let mut state = state.lock().unwrap();
            state.tip = 2;
            state.listed.clear();
            state.coins.remove(&a0);
            state.coins.insert(b0, (2, true));
        }
        let found = client.scan_utxos(&ScriptBuf::new()).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|u| (u.outpoint, u.height))
                .collect::<Vec<_>>(),
            [(b0, 9)]
        );
        // once the UTXO set no longer holds it, it is no longer asked about
        {
            let mut state = state.lock().unwrap();
            state.tip = 3;
            state.coins.clear();
        }
        assert!(client.scan_utxos(&ScriptBuf::new()).unwrap().is_empty());
        state.lock().unwrap().tip = 4;
        state.lock().unwrap().calls.clear();
        assert!(client.scan_utxos(&ScriptBuf::new()).unwrap().is_empty());
        assert!(!state.lock().unwrap().calls.iter().any(|m| m == "gettxout"));
        // no UTXO set scan at all
        assert!(!state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|m| m.ends_with("scantxoutset")));

        // the wallet's address check
        assert_eq!(client.wallet_watches("watched"), Ok(Some(true)));
        assert!(matches!(
            client.wallet_watches("other"),
            Err(BtcError::Decode(_))
        ));
        let plain = RpcClient::new(&url, "ksana", "ksana").unwrap();
        assert_eq!(plain.wallet_watches("watched"), Ok(None));
        assert!(RpcClient::new(&url, "ksana", "ksana")
            .unwrap()
            .with_wallet("a/b", ScriptBuf::new())
            .is_err());
    }

    #[test]
    fn malformed_responses_are_decode_errors() {
        let url = fake_bitcoind(|method, _| match method {
            "getblockchaininfo" => Ok(json!({"blocks": 3, "bestblockhash": "zz"})),
            "getblockhash" => Ok(json!("00".repeat(32))),
            "getblock" => Ok(json!("00")),
            _ => Err((-32601, "Method not found".into())),
        });
        let client = RpcClient::new(&url, "ksana", "ksana").unwrap();
        assert!(matches!(client.tip(), Err(BtcError::Decode(_))));
        assert_eq!(client.block_hash(1), Ok(Some([0; 32])));
        assert!(matches!(client.block(&[0; 32]), Err(BtcError::Decode(_))));
    }

    /// `getmempoolinfo` as the signet server's bitcoind answered it on 2026-10-18 (D-117); not known while bitcoind is
    /// still loading its mempool.
    #[test]
    fn the_mempool_info_is_its_virtual_size_and_minimum_fee_rate() {
        let answer = Arc::new(AtomicU8::new(0));
        let shown = answer.clone();
        let url = fake_bitcoind(
            move |method, _| match (method, shown.load(Ordering::SeqCst)) {
                ("getmempoolinfo", kind) => Ok(json!({
                    "loaded": kind != 1,
                    "size": 64,
                    "bytes": 314_541,
                    "usage": 1_389_856,
                    "total_fee": 0.06191758,
                    "maxmempool": 300_000_000,
                    "mempoolminfee": if kind == 2 { json!("0.000001") } else { json!(0.000001) },
                    "minrelaytxfee": 0.000001,
                    "incrementalrelayfee": 0.000001,
                    "unbroadcastcount": 0,
                    "fullrbf": true,
                })),
                _ => Err((-32601, "Method not found".into())),
            },
        );
        let client = RpcClient::new(&url, "ksana", "ksana").unwrap();
        assert_eq!(
            client.mempool_info(),
            Ok(Some(MempoolInfo {
                vsize: 314_541,
                min_feerate: 1,
            })),
            "0.1 sat/vB rounds up"
        );
        answer.store(1, Ordering::SeqCst);
        assert_eq!(client.mempool_info(), Ok(None), "still loading");
        answer.store(2, Ordering::SeqCst);
        assert!(matches!(client.mempool_info(), Err(BtcError::Decode(_))));
    }

    #[test]
    fn unreachable_node_is_a_local_fault() {
        // take a free port and close it right away; the connection will be refused.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = RpcClient::new(&format!("http://127.0.0.1:{port}"), "ksana", "ksana").unwrap();
        assert!(matches!(client.tip(), Err(BtcError::Rpc(_))));
        assert!(matches!(client.block_hash(0), Err(BtcError::Rpc(_))));
        assert!(matches!(
            client.broadcast(&tx_with_lock_time(0)),
            Err(BtcError::Rpc(_))
        ));
        assert!(format!("{client:?}").contains("127.0.0.1"));
    }
}
