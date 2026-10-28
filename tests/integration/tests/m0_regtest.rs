//! M0 integration tests: the regtest bitcoind started by `docker compose up` is configured per SPEC §7.1
//! (`-datacarriersize=100000 -txindex=1`) and can carry payloads in the KSN1 data format: path-C native transactions
//! in an OP_RETURN output, settlement and republish payloads in the envelope in the reveal transaction's witness
//! (D-081).
//!
//! Run: `tests/integration/run.sh` (`docker compose up` first, `down -v` at the end).

use bitcoin::hashes::Hash;
use ksana_btc::native::first_input;
use ksana_btc::settlement;
use ksana_crypto::{
    account_of, keypair_from_secret, sign_native_vmop, sign_vmop, verify_native_vmop, verify_vmop,
    SigError,
};
use ksana_integration_tests::{data_output, ksn1_script, parse_ksn1_script, Btc, MAX_CHUNK};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    compact, Action, BlockHeader, BtcAnchor, EntryData, FeeMode, OutPoint, Payload, SigScheme,
    VmOp, BLOCK_VERSION, DEVNET, VMOP_VERSION,
};

/// A signed Deploy op whose encoded length is exactly `encoded_len`; the signature of a path-C op (`first_input` is
/// `Some`) is bound to the first input of the carrying transaction (D-086).
fn signed_deploy_op(fee: FeeMode, encoded_len: usize, first_input: Option<&OutPoint>) -> VmOp {
    let kp = keypair_from_secret(&sha256(b"ksana/it/m0/user")).unwrap();
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: account_of(&kp),
        nonce: 0,
        fee,
        max_gas: 100_000,
        action: Action::Deploy {
            code: vec![],
            init_args: vec![],
            funds: Vec::new(),
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    let base = op.encode().unwrap().len();
    op.action = Action::Deploy {
        code: (0..encoded_len - base).map(|i| (i % 251) as u8).collect(),
        init_args: vec![],
        funds: Vec::new(),
    };
    op.sig = match first_input {
        None => sign_vmop(&op, &kp, &[7; 32]).unwrap(),
        Some(input) => sign_native_vmop(&op, input, &kp, &[7; 32]).unwrap(),
    };
    assert_eq!(op.encode().unwrap().len(), encoded_len);
    op
}

/// Broadcasts the transaction `tx` with its single KSN1 data output and mines 1 block to confirm it; returns the
/// payload read back from the chain and the transaction.
fn confirm_and_read_back(
    btc: &Btc,
    wallet: &Btc,
    tx: &bitcoin::Transaction,
) -> (Vec<u8>, bitcoin::Transaction) {
    wallet
        .test_accept(tx)
        .unwrap_or_else(|reason| panic!("transaction rejected: {reason}"));

    let rpc_txid = wallet.send(tx);
    let txid = tx.compute_txid();
    // DECISIONS D-001: the protocol stores the internal byte order; the RPC displays its reverse.
    let mut internal = txid.to_byte_array();
    assert_eq!(rpc_txid, txid.to_string());
    internal.reverse();
    assert_eq!(rpc_txid, hex::encode(internal));

    let addr = wallet.new_address();
    wallet.mine(1, &addr);
    let (confirmed, confirmations) = btc.get_confirmed_tx(&txid);
    assert!(confirmations >= 1, "transaction not confirmed");

    let data_outputs: Vec<Vec<u8>> = confirmed
        .output
        .iter()
        .filter_map(|o| parse_ksn1_script(&DEVNET.magic, &o.script_pubkey))
        .collect();
    assert_eq!(
        data_outputs.len(),
        1,
        "there should be exactly one KSN1 data output"
    );
    (data_outputs.into_iter().next().unwrap(), confirmed)
}

#[test]
fn bitcoind_is_regtest_with_txindex() {
    let btc = Btc::from_env();
    let chain = btc.call("getblockchaininfo", &[]);
    assert_eq!(chain["chain"], "regtest");
    let version = btc.call("getnetworkinfo", &[])["version"].as_u64().unwrap();
    assert!(
        version >= 280_000,
        "SPEC §12 requires Bitcoin Core ≥ 28.0, got {version}"
    );
    let index = btc.call("getindexinfo", &[]);
    assert!(
        index.get("txindex").is_some(),
        "txindex not enabled: {index}"
    );
}

/// The path-C native transaction format: a signed VmOp placed in `Payload::NativeOp`, several data chunks, unchanged
/// byte for byte after a round trip through the chain; the signature is bound to the carrying transaction's first
/// input (D-086) and is verified against the transaction read back from the chain.
#[test]
fn native_op_payload_roundtrips_through_regtest() {
    let btc = Btc::from_env();
    let wallet = btc.funded_wallet("it-m0-native-op");

    let (tx, op) = wallet.fund_native(
        &DEVNET,
        &|input| signed_deploy_op(FeeMode::BtcNative, 3_000, Some(input)),
        Vec::new(),
    );
    let payload = Payload::NativeOp { op: op.clone() }
        .encode(&DEVNET)
        .unwrap();
    assert!(
        payload.len() > 83,
        "must exceed the v28 default OP_RETURN limit of 83 bytes"
    );
    assert!(
        payload.len() > 2 * MAX_CHUNK,
        "should be split into several data chunks"
    );

    let (read_back, confirmed) = confirm_and_read_back(&btc, &wallet, &tx);
    assert_eq!(read_back, payload);
    match Payload::decode(&read_back, &DEVNET).unwrap() {
        Payload::NativeOp { op: decoded } => {
            assert_eq!(decoded, op);
            let input = first_input(&confirmed).expect("not a coinbase");
            assert_eq!(verify_native_vmop(&decoded, &input), Ok(()));
            // Without the outpoint, or with a different outpoint: fails
            assert_eq!(verify_vmop(&decoded), Err(SigError::FirstInputRequired));
            let other = OutPoint {
                vout: input.vout + 1,
                ..input
            };
            assert!(verify_native_vmop(&decoded, &other).is_err());
            assert_eq!(decoded.op_id().unwrap(), op.op_id().unwrap());
        }
        other => panic!("wrong payload type: {other:?}"),
    }
}

/// Puts the payload in the envelope of a commit / reveal transaction pair (D-081): the wallet pays the P2TR address of
/// the funding key and confirms it, `ksana-btc` assembles and signs the two transactions, both pass
/// testmempoolaccept as one package and are then broadcast in order and confirmed by mining 1 block; returns the
/// payload read back from the on-chain reveal transaction (the same recognition rules as the tracker) and the virtual
/// sizes of the two transactions.
fn roundtrip_through_envelope(btc: &Btc, wallet: &Btc, payload: &[u8]) -> (Vec<u8>, usize, usize) {
    let secp = bitcoin::key::Secp256k1::new();
    let key =
        bitcoin::key::Keypair::from_seckey_slice(&secp, &sha256(b"ksana/it/m0/envelope")).unwrap();
    let script = settlement::producer_script(&key.x_only_public_key().0);
    let funding = wallet.fund_and_sign(vec![bitcoin::TxOut {
        value: bitcoin::Amount::from_sat(1_000_000),
        script_pubkey: script.clone(),
    }]);
    wallet.send(&funding);
    let addr = wallet.new_address();
    wallet.mine(1, &addr);
    let funding_txid = funding.compute_txid();
    let vout = funding
        .output
        .iter()
        .position(|o| o.script_pubkey == script)
        .unwrap();
    let utxo = settlement::ProducerUtxo {
        outpoint: bitcoin::OutPoint::new(funding_txid, u32::try_from(vout).unwrap()),
        txout: funding.output[vout].clone(),
    };
    let draft = settlement::draft(
        &DEVNET,
        &[utxo],
        script.clone(),
        &key.x_only_public_key().0,
        payload,
        2,
    )
    .unwrap();
    let pair = settlement::finalize(&DEVNET, draft, payload, &key).unwrap();
    wallet
        .test_accept_package(&pair.txs())
        .unwrap_or_else(|reason| {
            panic!(
                "settlement transactions for a {}-byte payload rejected: {reason}",
                payload.len()
            )
        });
    wallet.send(&pair.commit);
    wallet.send(&pair.reveal);
    wallet.mine(1, &addr);
    let (reveal, confirmations) = btc.get_confirmed_tx(&pair.reveal.compute_txid());
    assert!(confirmations >= 1, "reveal transaction not confirmed");
    let read_back =
        ksana_btc::payload::envelope_payload(&DEVNET, &reveal).expect("envelope in the witness");
    (read_back, pair.commit.vsize(), reveal.vsize())
}

/// A settlement payload of exactly `MAX_BLOCK_DATA_BYTES` with `MAX_SETTLEMENT_HEADERS` headers placed in the reveal
/// transaction's witness (D-081): both transactions are standard, land on BTC and are read back in full; the payload
/// costs about 0.25 vB per byte.
#[test]
fn max_size_settlement_payload_is_standard() {
    let btc = Btc::from_env();
    let wallet = btc.funded_wallet("it-m0-max-payload");

    let mut headers = Vec::new();
    let mut parent = [0u8; 32];
    for height in 0..DEVNET.max_settlement_headers as u64 {
        let header = BlockHeader {
            version: BLOCK_VERSION,
            height,
            parents: vec![parent],
            btc_anchor: BtcAnchor {
                height: 101,
                hash: sha256(b"anchor"),
            },
            entries_root: sha256(&height.to_le_bytes()),
            // The state root differs from block to block: the compact encoding omits it when equal (D-079), and the
            // longest headers are wanted here
            state_root: sha256(&[b"state".as_slice(), &height.to_le_bytes()].concat()),
            producer: sha256(b"producer"),
            timestamp: 1_790_000_000 + height * 5,
            bits: 0x207f_ffff,
            nonce: height,
        };
        parent = header.hash();
        headers.push(header);
    }
    // Settlement = 1 (variant) + compact header sequence (D-075: the first writes all fields, each later one 78 bytes)
    // + body count (D-077) + entry count + compact entries (D-080): adjust the deploy code length by the difference
    // so that the payload is exactly the limit
    let headers_len = compact::encoded_len(&headers).unwrap();
    assert_eq!(
        headers_len,
        4 + 190 + (DEVNET.max_settlement_headers - 1) * 78
    );
    let build = |op_len: usize| {
        Payload::Settlement {
            headers: headers.clone(),
            entries: vec![EntryData::Sponsored {
                op: signed_deploy_op(FeeMode::Token { amount: 1 }, op_len, None),
            }],
            bodies: vec![],
        }
        .encode(&DEVNET)
        .unwrap()
    };
    let mut op_len = DEVNET.max_block_data_bytes - (1 + headers_len + 1 + 1 + 1);
    let mut payload = build(op_len);
    for _ in 0..4 {
        if payload.len() == DEVNET.max_block_data_bytes {
            break;
        }
        op_len = op_len + DEVNET.max_block_data_bytes - payload.len();
        payload = build(op_len);
    }
    assert_eq!(payload.len(), DEVNET.max_block_data_bytes);
    assert!(op_len <= DEVNET.max_op_bytes);

    let (read_back, commit_vsize, reveal_vsize) =
        roundtrip_through_envelope(&btc, &wallet, &payload);
    assert_eq!(read_back, payload);
    assert!(Payload::decode(&read_back, &DEVNET).is_ok());
    // The same payload in OP_RETURN costs about 1 vB / byte; in the witness about a quarter
    assert!(reveal_vsize < payload.len() / 4 + 400, "{reveal_vsize} vB");
    println!(
        "{}-byte settlement payload: commit transaction {commit_vsize} vB, reveal transaction {reveal_vsize} vB (about {} vB in OP_RETURN)",
        payload.len(),
        payload.len() + 140
    );
}

/// SPEC §7.1: Bitcoin Core before v30 treats only a single OP_RETURN output as standard -- the devnet's 28.x node
/// enforces this, so the data must be concatenated into the same OP_RETURN output.
#[test]
fn two_op_return_outputs_are_nonstandard() {
    let btc = Btc::from_env();
    let wallet = btc.funded_wallet("it-m0-multi-op-return");
    let script = ksn1_script(&DEVNET.magic, b"ksana");
    let tx = wallet.fund_and_sign(vec![data_output(script.clone()), data_output(script)]);
    assert_eq!(wallet.test_accept(&tx), Err("multi-op-return".to_string()));
}
