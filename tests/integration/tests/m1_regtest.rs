//! M1 integration tests (regtest bitcoind, `tests/integration/run.sh`):
//!
//! 1. BTC payment trigger contract (`contracts/examples/btc-payment-trigger`): a real regtest payment transaction, a
//!    real block header and a Merkle branch computed from the block's transaction list. The contract reads the block
//!    header through the host function `btc_header` (this test's `BtcView` queries bitcoind directly), verifies the
//!    inclusion proof and issues the asset according to the paid amount. Fails when the depth is less than
//!    `BTC_QUERY_DEPTH`; the same output cannot be claimed twice; an uncommitted beneficiary, a wrong output and a
//!    tampered proof all fail without changing the state.
//! 2. Path C: a signed VmOp placed in a KSN1 native transaction lands on BTC, is read back from the chain after
//!    confirmation and executed as a forced-inclusion entry; when the same op lands again, the nonce does not match
//!    and it is recorded as failed without changing the state.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use bitcoin::consensus::encode::{deserialize_hex, serialize};
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all::OP_RETURN;
use bitcoin::script::Builder;
use bitcoin::{Address, Amount, Block, BlockHash, Transaction, TxOut, Txid};
use ksana_btc::native::{first_input, native_op};
use ksana_crypto::{account_of, keypair_from_secret, sign_native_vmop, sign_vmop, Keypair};
use ksana_execution::builtin::{asset_id, contract_id};
use ksana_execution::keys::{self, AccountState, AssetMeta};
use ksana_execution::{BlockEnv, BtcView, Executor, GenesisConfig, MemState, OpStatus, Receipt};
use ksana_integration_tests::{data_output, ksn1_script, parse_ksn1_script, Btc};
use ksana_primitives::hash::{sha256, sha256d};
use ksana_primitives::{
    Action, BtcAnchor, Entry, FeeMode, OutPoint, Payload, SigScheme, VmOp, DEVNET, KSA_ASSET_ID,
    VMOP_VERSION,
};
use serde_json::json;

const GAS: u64 = 20_000_000;

struct User {
    kp: Keypair,
    account: [u8; 32],
}

fn user(seed: &str) -> User {
    let kp = keypair_from_secret(&sha256(format!("ksana/it/m1/{seed}").as_bytes())).unwrap();
    User {
        account: account_of(&kp),
        kp,
    }
}

fn signed(u: &User, nonce: u64, fee: FeeMode, max_gas: u64, action: Action) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee,
        max_gas,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[3; 32]).unwrap();
    op
}

/// A signed path-C op: the signature is bound to the carrying transaction's first input `first_input` (D-086).
fn signed_native(
    u: &User,
    nonce: u64,
    first_input: &OutPoint,
    max_gas: u64,
    action: Action,
) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::BtcNative,
        max_gas,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_native_vmop(&op, first_input, &u.kp, &[3; 32]).unwrap();
    op
}

/// A BTC view that queries bitcoind directly: returns the 80-byte header on the current active chain by height.
struct RpcBtc(Btc);

impl BtcView for RpcBtc {
    fn header(&self, height: u32) -> Result<Option<[u8; 80]>, String> {
        let hash = match self.0.try_call("getblockhash", &[json!(height)]) {
            Ok(hash) => hash,
            Err(_) => return Ok(None),
        };
        let hex = self.0.try_call("getblockheader", &[hash, json!(false)])?;
        let bytes = hex::decode(hex.as_str().ok_or("block header is not a string")?)
            .map_err(|e| e.to_string())?;
        bytes
            .try_into()
            .map(Some)
            .map_err(|_| "block header is not 80 bytes".to_string())
    }

    // This test only reads block headers: no unlock entries.
    fn block_settled(&self, _block_hash: &[u8; 32]) -> Result<bool, String> {
        Ok(false)
    }

    // No unlock entries: no carrier (D-095).
    fn carrier(&self, _block_hash: &[u8; 32]) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }

    // A path-C op's signature is verified against the carrying transaction's first input (D-086): read the
    // transaction from bitcoind; it must carry the op (D-100).
    fn native_input(&self, txid: &[u8; 32], op: &VmOp) -> Result<Option<OutPoint>, String> {
        let txid = Txid::from_byte_array(*txid);
        let hex = self.0.try_call(
            "getrawtransaction",
            &[json!(txid.to_string()), json!(false)],
        )?;
        let tx: Transaction = deserialize_hex(hex.as_str().ok_or("transaction is not a string")?)
            .map_err(|e| e.to_string())?;
        Ok(native_op(&DEVNET, &tx)
            .filter(|carried| carried == op)
            .and_then(|_| first_input(&tx)))
    }

    // This test's contract does not read the BTC transaction carrying the op.
    fn native_outputs(&self, _txid: &[u8; 32]) -> Result<Vec<(u64, Vec<u8>)>, String> {
        Err("the m1 test does not read native transactions".into())
    }
}

/// A fast lane executing block by block on in-memory state; the BTC anchor of each block is given by the caller.
struct Fast {
    exec: Executor,
    state: MemState,
    btc: Arc<RpcBtc>,
    height: u64,
    parent: [u8; 32],
    producer: [u8; 32],
    nonces: BTreeMap<[u8; 32], u64>,
}

impl Fast {
    fn new(allocations: Vec<([u8; 32], u128)>) -> Self {
        let mut state = MemState::default();
        state.apply(&GenesisConfig::devnet(allocations).writes().unwrap());
        Fast {
            exec: Executor::new(DEVNET).unwrap(),
            state,
            btc: Arc::new(RpcBtc(Btc::from_env())),
            height: 0,
            parent: sha256(b"ksana/it/m1/genesis"),
            producer: user("producer").account,
            nonces: BTreeMap::new(),
        }
    }

    fn block(&mut self, anchor: &BtcAnchor, entries: Vec<Entry>) -> Vec<Receipt> {
        // The blocks here contain no unlock entries, so no unlockable segment is needed
        let env = BlockEnv {
            height: self.height + 1,
            btc_anchor: anchor.clone(),
            producer: self.producer,
            timestamp: 1_700_000_000 + self.height + 1,
            rules: DEVNET.rules(self.height + 1),
            unlockable: Vec::new(),
        };
        let out = self
            .exec
            .execute_block(
                Arc::new(self.state.clone()),
                self.btc.clone(),
                &env,
                &entries,
            )
            .expect("the block should be valid");
        self.state.apply(&out.writes);
        self.height = env.height;
        self.parent = sha256(&[self.parent.as_slice(), &self.height.to_le_bytes()].concat());
        out.receipts
    }

    /// Path B: a single op in a block of its own.
    fn sponsored(&mut self, anchor: &BtcAnchor, u: &User, action: Action) -> Receipt {
        let nonce = self.nonces.entry(u.account).or_insert(0);
        let op = signed(u, *nonce, FeeMode::Token { amount: 5 }, GAS, action);
        *nonce += 1;
        self.block(anchor, vec![Entry::Sponsored { op }]).remove(0)
    }

    fn get<T: borsh::BorshDeserialize>(&self, key: &[u8]) -> Option<T> {
        self.state.0.get(key).map(|v| borsh::from_slice(v).unwrap())
    }

    fn balance(&self, asset: &[u8; 32], account: &[u8; 32]) -> u128 {
        self.get(&keys::balance(asset, account)).unwrap_or(0)
    }

    fn nonce(&self, account: &[u8; 32]) -> u64 {
        self.get::<AccountState>(&keys::account(account))
            .map(|a| a.nonce)
            .unwrap_or(0)
    }
}

/// The anchor at a height on the regtest chain (hash in internal byte order).
fn anchor_at(btc: &Btc, height: u32) -> BtcAnchor {
    let hash = btc.call("getblockhash", &[json!(height)]);
    let hash = BlockHash::from_str(hash.as_str().unwrap()).unwrap();
    BtcAnchor {
        height,
        hash: hash.to_byte_array(),
    }
}

/// The Merkle branch of the `index`-th transaction in a block: internal byte order, starting from the leaf side; an
/// odd level duplicates its last node.
fn merkle_branch(txids: &[[u8; 32]], mut index: usize) -> ([u8; 32], Vec<[u8; 32]>) {
    let mut level = txids.to_vec();
    let mut branch = Vec::new();
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }
        branch.push(level[index ^ 1]);
        level = level
            .chunks(2)
            .map(|pair| sha256d(&[pair[0], pair[1]].concat()))
            .collect();
        index /= 2;
    }
    (level[0], branch)
}

/// The transaction serialization without witness data (the preimage of the txid).
fn without_witness(tx: &Transaction) -> Vec<u8> {
    let mut stripped = tx.clone();
    for input in &mut stripped.input {
        input.witness.clear();
    }
    let bytes = serialize(&stripped);
    assert_eq!(sha256d(&bytes), tx.compute_txid().to_byte_array());
    bytes
}

fn commitment_output(beneficiary: &[u8; 32]) -> TxOut {
    data_output(
        Builder::new()
            .push_opcode(OP_RETURN)
            .push_slice(beneficiary)
            .into_script(),
    )
}

fn failure_reason(r: &Receipt) -> String {
    match &r.status {
        OpStatus::Failed(reason) => reason.clone(),
        other => panic!("the op should fail: {other:?}"),
    }
}

#[test]
fn btc_payment_trigger_verifies_real_regtest_payment() {
    let btc = Btc::from_env();
    let wallet = btc.funded_wallet("it-m1-trigger");
    let (deployer, alice, mallory) = (user("deployer"), user("alice"), user("mallory"));

    // The watched locking script: an address of the merchant wallet
    let merchant = btc.wallet("it-m1-merchant");
    let watch = Address::from_str(&merchant.new_address())
        .unwrap()
        .assume_checked()
        .script_pubkey();

    // The payment transaction: pays the watched script and commits to the beneficiary alice with an OP_RETURN. A few
    // unrelated transactions go into the same block so that the Merkle branch has more than one level.
    let paid = Amount::from_sat(123_456);
    let payment = wallet.fund_and_sign(vec![
        TxOut {
            value: paid,
            script_pubkey: watch.clone(),
        },
        commitment_output(&alice.account),
    ]);
    wallet.send(&payment);
    for _ in 0..5 {
        let other = Address::from_str(&merchant.new_address())
            .unwrap()
            .assume_checked();
        let tx = wallet.fund_and_sign(vec![TxOut {
            value: Amount::from_sat(10_000),
            script_pubkey: other.script_pubkey(),
        }]);
        wallet.send(&tx);
    }
    let miner = wallet.new_address();
    let block_hash = wallet.mine(1, &miner).remove(0);
    let block = btc.call("getblock", &[json!(block_hash), json!(1)]);
    let height = u32::try_from(block["height"].as_u64().unwrap()).unwrap();
    let txids: Vec<[u8; 32]> = block["tx"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| Txid::from_str(t.as_str().unwrap()).unwrap().to_byte_array())
        .collect();
    assert!(
        txids.len() >= 5,
        "the block should have several transactions: {}",
        txids.len()
    );
    let payment_txid = payment.compute_txid().to_byte_array();
    let tx_index = txids.iter().position(|t| *t == payment_txid).unwrap();
    let (root, branch) = merkle_branch(&txids, tx_index);
    let header = RpcBtc(Btc::from_env()).header(height).unwrap().unwrap();
    assert_eq!(
        &header[36..68],
        root.as_slice(),
        "computed Merkle root does not match the block header"
    );
    assert!(branch.len() >= 3);
    let raw_tx = without_witness(&payment);
    // The same block's coinbase transaction and its branch: pins the Merkle tree depth (D-072)
    let raw_block = btc.call("getblock", &[json!(block_hash), json!(0)]);
    let full: Block = deserialize_hex(raw_block.as_str().unwrap()).unwrap();
    let coinbase = without_witness(&full.txdata[0]);
    assert_eq!(sha256d(&coinbase), txids[0]);
    let (_, coinbase_branch) = merkle_branch(&txids, 0);
    assert_eq!(coinbase_branch.len(), branch.len());
    // fundrawtransaction inserts the change output at a random position: find the output index by locking script
    let output_index = |script: &bitcoin::Script| {
        u32::try_from(
            payment
                .output
                .iter()
                .position(|o| o.script_pubkey.as_script() == script)
                .unwrap(),
        )
        .unwrap()
    };
    let pay_vout = output_index(&watch);
    let commit_vout = output_index(&commitment_output(&alice.account).script_pubkey);

    // Mine BTC_QUERY_DEPTH more blocks: the header becomes readable only when anchored at height + depth
    let depth = DEVNET.btc_query_depth;
    wallet.mine(u64::from(depth), &miner);
    let shallow = anchor_at(&btc, height + depth - 1);
    let deep = anchor_at(&btc, height + depth);

    let mut fast = Fast::new(vec![
        (deployer.account, 1_000),
        (alice.account, 1_000),
        (mallory.account, 1_000),
    ]);
    let code = ksana_test_contracts::BTC_PAYMENT_TRIGGER;
    let trigger = contract_id(&deployer.account, 0, &sha256(code));
    let sub_id = sha256(b"btc-receipt");
    let units_per_sat = 1_000u128;
    let init = borsh::to_vec(&(
        watch.to_bytes(),
        sub_id,
        ("BTC receipt".to_string(), "BTCR".to_string(), 11u8),
        units_per_sat,
    ))
    .unwrap();
    let r = fast.sponsored(
        &deep,
        &deployer,
        Action::Deploy {
            code: code.to_vec(),
            init_args: init,
            funds: Vec::new(),
        },
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let asset = asset_id(&trigger, &sub_id);

    let claim = |vout: u32, beneficiary: [u8; 32], branch: &[[u8; 32]]| Action::Call {
        contract: trigger,
        method: "claim".into(),
        args: borsh::to_vec(&(
            height,
            raw_tx.clone(),
            u32::try_from(tx_index).unwrap(),
            branch.to_vec(),
            coinbase.clone(),
            coinbase_branch.clone(),
            vout,
            beneficiary,
        ))
        .unwrap(),
        funds: Vec::new(),
    };

    // Insufficient depth: the host refuses to read the header and the whole op fails
    let r = fast.sponsored(&shallow, &alice, claim(pay_vout, alice.account, &branch));
    assert!(
        failure_reason(&r).contains("insufficient BTC block header depth"),
        "{r:?}"
    );
    assert_eq!(fast.balance(&asset, &alice.account), 0);

    // A beneficiary not committed in the transaction (front-running)
    let r = fast.sponsored(&deep, &mallory, claim(pay_vout, mallory.account, &branch));
    assert!(failure_reason(&r).contains("beneficiary"), "{r:?}");
    // The beneficiary commitment output (OP_RETURN) does not pay the watched script
    let r = fast.sponsored(&deep, &alice, claim(commit_vout, alice.account, &branch));
    assert!(failure_reason(&r).contains("watched script"), "{r:?}");
    // A tampered Merkle branch
    let mut bad_branch = branch.clone();
    bad_branch[1][0] ^= 1;
    let r = fast.sponsored(&deep, &alice, claim(pay_vout, alice.account, &bad_branch));
    assert!(failure_reason(&r).contains("not included"), "{r:?}");
    assert_eq!(fast.balance(&asset, &alice.account), 0);
    assert!(fast.get::<AssetMeta>(&keys::asset(&asset)).is_none());

    // Deep enough: the claim succeeds and the asset is issued according to the amount
    let r = fast.sponsored(&deep, &mallory, claim(pay_vout, alice.account, &branch));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let amount = u128::from(paid.to_sat()) * units_per_sat;
    assert_eq!(borsh::from_slice::<u128>(&r.output).unwrap(), amount);
    assert_eq!(fast.balance(&asset, &alice.account), amount);
    assert_eq!(fast.balance(&asset, &mallory.account), 0);
    let meta: AssetMeta = fast.get(&keys::asset(&asset)).unwrap();
    assert_eq!(meta.total_supply, amount);
    assert_eq!(meta.issuer, trigger);
    assert_eq!(meta.name, "BTC receipt");
    assert_eq!(meta.symbol, "BTCR");
    assert_eq!(r.events.len(), 1);
    assert_eq!(
        r.events[0].data,
        borsh::to_vec(&(
            "claim".to_string(),
            payment_txid,
            pay_vout,
            alice.account,
            asset,
            amount
        ))
        .unwrap()
    );

    // The same output cannot be claimed twice
    let r = fast.sponsored(&deep, &alice, claim(pay_vout, alice.account, &branch));
    assert!(failure_reason(&r).contains("already claimed"), "{r:?}");
    assert_eq!(fast.balance(&asset, &alice.account), amount);
    let claimed = Action::Call {
        contract: trigger,
        method: "is_claimed".into(),
        args: borsh::to_vec(&(payment_txid, pay_vout)).unwrap(),
        funds: Vec::new(),
    };
    let r = fast.sponsored(&deep, &alice, claimed);
    assert!(borsh::from_slice::<bool>(&r.output).unwrap());
}

#[test]
fn native_op_from_regtest_executes_as_forced_entry() {
    let btc = Btc::from_env();
    let wallet = btc.funded_wallet("it-m1-native");
    let (x, y) = (user("native-x"), user("native-y"));

    let action = Action::AssetTransfer {
        asset: KSA_ASSET_ID,
        to: y.account,
        amount: 250,
    };
    // The first one: the signature is bound to its first input (D-086)
    let (first, op) = wallet.fund_native(
        &DEVNET,
        &|input| signed_native(&x, 0, input, 100_000, action.clone()),
        Vec::new(),
    );
    wallet.send(&first);
    // The second one: someone else copies the same op verbatim into their own transaction (replay)
    let payload = Payload::NativeOp { op: op.clone() }
        .encode(&DEVNET)
        .unwrap();
    let copy = wallet.fund_and_sign(vec![data_output(ksn1_script(&DEVNET.magic, &payload))]);
    wallet.send(&copy);
    let native_txids = [first.compute_txid(), copy.compute_txid()];
    let miner = wallet.new_address();
    wallet.mine(u64::from(DEVNET.native_confirmations), &miner);
    let tip = u32::try_from(btc.call("getblockcount", &[]).as_u64().unwrap()).unwrap();

    // Read the native transactions back from the chain and form forced-inclusion entries
    let entries: Vec<Entry> = native_txids
        .iter()
        .map(|txid| {
            let (tx, confirmations) = btc.get_confirmed_tx(txid);
            assert!(confirmations >= u64::from(DEVNET.native_confirmations));
            let data = tx
                .output
                .iter()
                .find_map(|o| parse_ksn1_script(&DEVNET.magic, &o.script_pubkey))
                .expect("KSN1 data output");
            let Payload::NativeOp { op } = Payload::decode(&data, &DEVNET).unwrap() else {
                panic!("not a native transaction payload");
            };
            Entry::Forced {
                btc_txid: txid.to_byte_array(),
                op,
            }
        })
        .collect();

    let mut fast = Fast::new(vec![(x.account, 1_000)]);
    let receipts = fast.block(&anchor_at(&btc, tip), entries);
    assert_eq!(receipts[0].status, OpStatus::Success, "{:?}", receipts[0]);
    assert_eq!(receipts[0].op_id, Some(op.op_id().unwrap()));
    // The copied one: the signature is bound to the first one's input and fails authentication (not a nonce mismatch,
    // and no nonce is consumed)
    assert!(
        failure_reason(&receipts[1]).contains("signature"),
        "{:?}",
        receipts[1]
    );
    assert_eq!(fast.balance(&KSA_ASSET_ID, &x.account), 750);
    assert_eq!(fast.balance(&KSA_ASSET_ID, &y.account), 250);
    assert_eq!(fast.nonce(&x.account), 1);
    // Path C pays the producer no token fee: this block has no sponsored fee record (the block reward is paid at
    // unlock, D-079)
    assert_eq!(
        fast.get::<ksana_execution::keys::LockedFee>(&keys::locked_fee(1, 0)),
        None
    );
}
