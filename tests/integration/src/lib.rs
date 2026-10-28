//! Integration test utilities: connect to the devnet's regtest bitcoind (`devnet/docker-compose.yml`).
//!
//! The connection parameters come from environment variables, with defaults matching the compose file:
//! `KSANA_BTC_RPC_URL` (`http://127.0.0.1:18443`), `KSANA_BTC_RPC_USER` / `KSANA_BTC_RPC_PASS` (`ksana`).
//! This is test code: an RPC failure panics directly with context.

use bitcoin::consensus::encode::{deserialize_hex, serialize, serialize_hex};
use bitcoin::opcodes::all::OP_RETURN;
use bitcoin::script::{Builder, Instruction, PushBytesBuf};
use bitcoin::{absolute, transaction, Amount, Script, ScriptBuf, Transaction, TxOut, Txid};
use corepc_client::client_sync::v28::Client;
use corepc_client::client_sync::Auth;
use ksana_btc::native::{first_input, native_op, with_native_op};
use ksana_primitives::{OutPoint, Params, Payload, VmOp};
use serde_json::{json, Value};

/// SPEC §7.1: each OP_RETURN data chunk is at most 520 bytes.
pub const MAX_CHUNK: usize = 520;

/// Fee rate (sat/vB) used by the test transactions, not below `MIN_SETTLEMENT_FEERATE`.
pub const TEST_FEE_RATE: u64 = 2;
/// Minimum available balance (sats) guaranteed by [`Btc::funded_wallet`].
const MIN_WALLET_SAT: u64 = 5_000_000;

/// A bitcoind JSON-RPC connection (optionally bound to a wallet).
pub struct Btc {
    base_url: String,
    user: String,
    pass: String,
    client: Client,
}

impl Btc {
    /// Connects to the node according to the environment variables.
    pub fn from_env() -> Btc {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        Btc::connect(
            &env("KSANA_BTC_RPC_URL", "http://127.0.0.1:18443"),
            &env("KSANA_BTC_RPC_USER", "ksana"),
            &env("KSANA_BTC_RPC_PASS", "ksana"),
        )
    }

    fn connect(url: &str, user: &str, pass: &str) -> Btc {
        let client = Client::new_with_auth(url, Auth::UserPass(user.into(), pass.into()))
            .unwrap_or_else(|e| panic!("connecting to {url}: {e}"));
        Btc {
            base_url: url.trim_end_matches('/').to_string(),
            user: user.into(),
            pass: pass.into(),
            client,
        }
    }

    /// Calls an RPC; panics on failure.
    pub fn call(&self, method: &str, args: &[Value]) -> Value {
        self.try_call(method, args)
            .unwrap_or_else(|e| panic!("RPC {method} {args:?} failed: {e}"))
    }

    /// Calls an RPC, returning the error message instead of panicking.
    pub fn try_call(&self, method: &str, args: &[Value]) -> Result<Value, String> {
        self.client
            .call::<Value>(method, args)
            .map_err(|e| e.to_string())
    }

    /// Creates or loads the wallet named `name` and returns a connection bound to it.
    pub fn wallet(&self, name: &str) -> Btc {
        let loaded = self.call("listwallets", &[]);
        let is_loaded = loaded
            .as_array()
            .is_some_and(|ws| ws.iter().any(|w| w == name));
        if !is_loaded {
            let on_disk = self.call("listwalletdir", &[]);
            let exists = on_disk["wallets"]
                .as_array()
                .is_some_and(|ws| ws.iter().any(|w| w["name"] == name));
            if exists {
                self.call("loadwallet", &[json!(name)]);
            } else {
                self.call("createwallet", &[json!(name)]);
            }
        }
        Btc::connect(
            &format!("{}/wallet/{name}", self.base_url),
            &self.user,
            &self.pass,
        )
    }

    /// A new address of the wallet.
    pub fn new_address(&self) -> String {
        self.call("getnewaddress", &[])
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Mines `n` blocks to `address` and returns the block hashes.
    pub fn mine(&self, n: u64, address: &str) -> Vec<String> {
        let hashes = self.call("generatetoaddress", &[json!(n), json!(address)]);
        hashes
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h.as_str().unwrap().to_string())
            .collect()
    }

    /// Returns a wallet with spendable UTXOs; if it has none, mines 101 blocks to it so that the first coinbase
    /// matures.
    pub fn funded_wallet(&self, name: &str) -> Btc {
        let wallet = self.wallet(name);
        let spendable = wallet.call("listunspent", &[json!(1)]);
        if spendable.as_array().is_none_or(|u| u.is_empty()) {
            let addr = wallet.new_address();
            wallet.mine(101, &addr);
        }
        // On a long-lived local regtest chain the block reward has halved many times (every 150 blocks), so one
        // mature coinbase may not be enough: keep mining until the available balance is at least MIN_WALLET_SAT (on a
        // fresh chain the first coinbase alone far exceeds it)
        for _ in 0..50 {
            let trusted = wallet.call("getbalances", &[])["mine"]["trusted"]
                .as_f64()
                .unwrap_or(0.0);
            if trusted * 1e8 >= MIN_WALLET_SAT as f64 {
                break;
            }
            let addr = wallet.new_address();
            wallet.mine(10, &addr);
        }
        wallet
    }

    /// Funds `outputs` from the wallet (at [`TEST_FEE_RATE`]) and signs, returning the complete transaction.
    pub fn fund_and_sign(&self, outputs: Vec<TxOut>) -> Transaction {
        self.sign_with_wallet(&self.fund(outputs))
    }

    /// Funds `outputs` from the wallet (at [`TEST_FEE_RATE`]), returning the transaction with its inputs not yet
    /// signed.
    pub fn fund(&self, outputs: Vec<TxOut>) -> Transaction {
        // A zero-input transaction must be serialized in the legacy format: rust-bitcoin writes the segwit marker for
        // a zero-input transaction, and Bitcoin Core rejects "segwit format without any witness". The third
        // parameter iswitness=false makes Core parse it in the legacy format.
        let mut unsigned = Vec::new();
        unsigned.extend(transaction::Version::TWO.0.to_le_bytes());
        unsigned.push(0); // 0 inputs
        unsigned.extend(serialize(&outputs));
        unsigned.extend(absolute::LockTime::ZERO.to_consensus_u32().to_le_bytes());
        let funded = self.call(
            "fundrawtransaction",
            &[
                json!(hex::encode(unsigned)),
                json!({"fee_rate": TEST_FEE_RATE}),
                json!(false),
            ],
        );
        deserialize_hex(funded["hex"].as_str().unwrap()).unwrap()
    }

    /// The wallet signs all inputs of `tx`, returning the complete transaction.
    pub fn sign_with_wallet(&self, tx: &Transaction) -> Transaction {
        let signed = self.call("signrawtransactionwithwallet", &[json!(serialize_hex(tx))]);
        assert_eq!(signed["complete"], true, "signing incomplete: {signed}");
        deserialize_hex(signed["hex"].as_str().unwrap()).unwrap()
    }

    /// A path-C native transaction funded by the wallet (D-086: the op's signature is bound to the first input of the
    /// carrying transaction): first fund with the placeholder op that `sign` gives for the all-zero outpoint (same
    /// encoded length; a BIP322 ECDSA signature has variable length, so the placeholder payload reserves 2 extra
    /// bytes), then sign the op against the first input and replace the KSN1 data output, and finally let the wallet
    /// sign the inputs. `payments` follow the data output. Returns the signed transaction (not broadcast) and the op
    /// it carries.
    pub fn fund_native(
        &self,
        params: &Params,
        sign: &dyn Fn(&OutPoint) -> VmOp,
        payments: Vec<TxOut>,
    ) -> (Transaction, VmOp) {
        let placeholder = sign(&OutPoint {
            txid: [0; 32],
            vout: 0,
        });
        let mut payload = Payload::NativeOp { op: placeholder }
            .encode(params)
            .unwrap();
        payload.extend([0u8; 2]);
        let mut outputs = vec![data_output(ksn1_script(&params.magic, &payload))];
        outputs.extend(payments);
        let funded = self.fund(outputs);
        let input = first_input(&funded).expect("the funded transaction has inputs");
        let op = sign(&input);
        let bound = with_native_op(&funded, params, &op).unwrap();
        let tx = self.sign_with_wallet(&bound);
        assert_eq!(
            native_op(params, &tx),
            Some(op.clone()),
            "the constructed transaction should be recognized as a native transaction"
        );
        assert_eq!(
            first_input(&tx),
            Some(input),
            "wallet signing does not change the first input"
        );
        (tx, op)
    }

    /// Result of `testmempoolaccept`: `Ok(())` or the rejection reason.
    pub fn test_accept(&self, tx: &Transaction) -> Result<(), String> {
        let res = self.call("testmempoolaccept", &[json!([serialize_hex(tx)])]);
        let entry = &res[0];
        if entry["allowed"] == true {
            Ok(())
        } else {
            Err(entry["reject-reason"].as_str().unwrap_or("?").to_string())
        }
    }

    /// Broadcasts a transaction and returns the txid given by the RPC (hex in display byte order).
    /// `testmempoolaccept`: `txs` are checked in order (parents first) as one package; `Ok` when all are accepted,
    /// otherwise the first rejection reason.
    pub fn test_accept_package(&self, txs: &[&Transaction]) -> Result<(), String> {
        let raw: Vec<String> = txs.iter().map(|tx| serialize_hex(*tx)).collect();
        let result = self.call("testmempoolaccept", &[json!(raw)]);
        for entry in result.as_array().unwrap() {
            if entry["allowed"].as_bool() != Some(true) {
                return Err(entry["reject-reason"]
                    .as_str()
                    .or(entry["package-error"].as_str())
                    .unwrap_or("unknown reason")
                    .to_string());
            }
        }
        Ok(())
    }

    pub fn send(&self, tx: &Transaction) -> String {
        self.call("sendrawtransaction", &[json!(serialize_hex(tx))])
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Reads a confirmed transaction and its confirmation count via txindex.
    pub fn get_confirmed_tx(&self, txid: &Txid) -> (Transaction, u64) {
        let verbose = self.call("getrawtransaction", &[json!(txid.to_string()), json!(true)]);
        let tx = deserialize_hex(verbose["hex"].as_str().unwrap()).unwrap();
        let confirmations = verbose["confirmations"].as_u64().unwrap_or(0);
        (tx, confirmations)
    }
}

/// The data format of SPEC §7.1: `OP_RETURN <magic> <chunk1> <chunk2> …`, each data chunk at most 520 bytes.
///
/// Test-only implementation; the real settlement construction and native transaction scanning are implemented in
/// M3's `ksana-btc`.
pub fn ksn1_script(magic: &[u8; 4], payload: &[u8]) -> ScriptBuf {
    let mut builder = Builder::new()
        .push_opcode(OP_RETURN)
        .push_slice(PushBytesBuf::from(*magic));
    for chunk in payload.chunks(MAX_CHUNK) {
        builder = builder.push_slice(PushBytesBuf::try_from(chunk.to_vec()).unwrap());
    }
    builder.into_script()
}

/// If `script` is a KSN1 data output starting with `magic`, returns the payload formed by concatenating the data
/// chunks in order.
pub fn parse_ksn1_script(magic: &[u8; 4], script: &Script) -> Option<Vec<u8>> {
    let mut ins = script.instructions_minimal();
    match ins.next()? {
        Ok(Instruction::Op(op)) if op == OP_RETURN => {}
        _ => return None,
    }
    match ins.next()? {
        Ok(Instruction::PushBytes(p)) if p.as_bytes() == magic => {}
        _ => return None,
    }
    let mut payload = Vec::new();
    for i in ins {
        match i {
            Ok(Instruction::PushBytes(p)) if p.len() <= MAX_CHUNK => {
                payload.extend_from_slice(p.as_bytes())
            }
            _ => return None,
        }
    }
    Some(payload)
}

/// A single OP_RETURN data output (amount 0).
pub fn data_output(script: ScriptBuf) -> TxOut {
    TxOut {
        value: Amount::ZERO,
        script_pubkey: script,
    }
}
